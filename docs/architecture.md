# Kourier architecture

Kourier (working name) is a native Linux desktop messenger in Rust. It replaces the Electron
Signal Desktop app. Signal is the first network. Threema and WhatsApp backends are planned, so
the core is protocol-agnostic from day one.

- Status: design baseline for milestone 1 (M1), written 2026-10-05 and revised the same day after
  an adversarial review (23 issues; see the Review log at the end). Implemented so far (phases A-J,
  2026-10-06/07): S0-S15 and the parts of S16 that need no real account, all on the Signal mock
  server and the fake network. Phase H added what §1.1 goal 2 asks for: any number of files of
  any kind in one send, split into the messages every Signal client shows in full, and an audit
  of every limit copied from Signal Desktop (one table in §13 phase H). Phase I built S16's
  measuring tool (`cargo xtask perf`), measured §1.3 and fixed what the first measurement found.
  It missed the two memory budgets the design had then; the budgets of §1.3 are the ones the
  user confirmed afterwards, and the two quick runs of phase J miss none of them. Phase I also
  built `cargo xtask install` and `uninstall`, the AppStream metainfo, and
  most of phase H's open list: packing by file format, sends that can be followed by a request
  id, typed refusals, custom timers, "send as original file", clearing and deleting a chat and
  deleting one attachment from Kourier, read receipts in groups, identity merges, downloads
  that resume, a challenge that survives a restart. Phase J built the two features the user
  decided on 2026-10-07. The first is a setting for disappearing messages whose time is up
  while no server has confirmed this computer's clock: hide them until the time is confirmed
  (the default), keep them until then, or erase by the local clock as Signal Desktop does
  (§9.6). The second is a folder given to the composer, which asks whether to add the files
  inside or to send the folder as one .zip (§8.3). Phase J also sends the core's memory
  release when the hidden window is let go (measured: a few MB, §1.3) and closed leftovers of
  phase I's list: a Signal login is stopped instead of aborted and a device linked for it is
  removed, the custom timer's four-week bound is gone under the limits rule, a challenge hold
  on jobs ends with its notice. What was built differently from §1-§11 is
  in §13, with an "Open for the next phase" list per phase; the newest lists (phases J and
  K) are the current ones. With phase J the build steps of M1 and the two features decided
  since are done on the mock; the steps that follow need the user. Phase K made the first of
  them safe to take: the fixes an audit of the production path asked for (an account bound to
  its server, Desktop's capabilities, a guard on the account a first link finds, a lock per
  profile, no wipe on a wrong key, redacted logs, no core dumps) and `cargo xtask smoke`, the
  step-by-step run on a profile that keeps its login (`docs/smoke-run.md`).
  Next: `cargo xtask ci`, `cargo xtask demo` and a full `cargo xtask perf` on the merged
  phase J tree (each lane ran only its own suites), then the steps that need the user: the
  questions marked "to decide" in §13 phase J's open list (first: what happens to the due
  messages of an account that has no running backend), the real-server smoke run and a week of
  dogfooding (a dedicated test account and the user's OK in that session), the manual Plasma
  checklist (§10.6 and the lists in §13), `cargo xtask install --user` on the user's machine,
  `cargo xtask perf --no-mock` after a login, and the FOSS release track of §14 (rename
  first).
- Inputs: eight research reports (cited as `R:<name> §n`; they are working notes kept
  outside this repository), the upstream sources they cite, and two build spikes.
- Legend: **[V]** verified in source, on this machine or in a spike artifact. **[I]** inference,
  estimate or design choice that is not yet proven by running code. Every section below that says
  "Decision" is a design choice; the reasons follow it.

Paths: `SD/` = a checkout of `signalapp/Signal-Desktop` (v8.32.0-alpha.1, `832279c2`, 2026-10-01),
`LS/` = `signalapp/libsignal` (tag v0.104.0, `257105c5`, 2026-10-02),
`TD/` = `threema-ch/threema-desktop`, `WA/` = the `whatsapp-rust` crate's repository.

---

## 0. Decisions at a glance

| Topic | Decision | Main reason |
|---|---|---|
| Signal foundation | **Own Signal service layer (`kourier-signal`) directly on libsignal v0.104.0, including `libsignal-net` and `libsignal-net-chat`.** No presage, no libsignal-service-rs dependency. lss is a reference and a source of AGPL code we may port. | Ack-after-commit, sender keys with Group Send Endorsements, gRPC switch-over and mock-server testability come with libsignal-net and are missing in lss (§6.1) |
| Core ↔ backend boundary | Per-account actor. Commands in over a bounded `mpsc`, events out as `Envelope{events, ack}`. One small sync `Network` trait per network. All crossing data is `Send + 'static` serde data | Absorbs `!Send` libsignal stores, `Send + Sync` whatsapp-rust and sans-IO libthreema (R:multi-protocol-prior-art §0.2) |
| Delivery semantics | At-least-once, idempotent ingest keyed by a structured message id; nothing is acked to a server before it is durable. **Transactional outbox:** every backend transaction that changes state also stores the core events it implies in `core_outbox` (backend.db); a forwarder ships them and deletes them after the core commit | All three stacks work this way (R:multi-protocol-prior-art §0.3); the outbox closes the crash window between a backend commit and the core commit (§4.1, §5.6) |
| Message ids | `NetMsgId { author: Option<UserId>, local }` in two columns; matching scope per network (Signal account-wide, WhatsApp/Threema per conversation) | Threema references carry no author; WhatsApp own ids must not depend on PN/LID addressing (§3.1) |
| Persistence | `core.db` (one SQLCipher file, all accounts, core schema) + `accounts/<uuid>/backend.db` (one per account, backend-owned) + `accounts/<uuid>/blobs/` | Exclusive writer per backend, per-account shredding, unified queries in core (§5) |
| DB engine | rusqlite 0.38 + SQLCipher 4 with a RustCrypto crypto provider (`libsqlite3-sys` 0.36 from `boxdot/rusqlite` rev `a2f39391`, as gurk ships it) | OpenSSL SQLCipher clashes with libsignal-net's BoringSSL in one process; the custom-crypto build avoids OpenSSL (§5.3) |
| Keys | Random 32-byte master key in the Secret Service (oo7) keys `core.db`; each account's random `db_key` is wrapped in `accounts/<uuid>/key.wrap` under a KEK from the master key and a per-account Secret Service item; deleting that item shreds the account; passphrase (Argon2id) fallback; never plaintext | Signal Desktop keeps its DB key in `safeStorage` (`SD/app/main.main.ts:1717-1825`); wrap files let `core.db` be rebuilt without losing accounts (§5.2) |
| Attachments at rest | Encrypted per file (own streaming AEAD format), plaintext only in `$XDG_RUNTIME_DIR` while media is playing | Matches Signal Desktop (`SD/app/attachment_channel.main.ts:608`) |
| UI | Qt 6.11 QML + Kirigami through cxx-qt 0.10, one bridge crate (`kourier-qt`), all view logic in a Qt-free crate (`kourier-viewmodel`) | R:qt-kirigami §0, §9; the spike rendered a Kirigami window from Rust [V] |
| Message text | Read-only `TextEdit` with RichText generated in Rust from a tag whitelist; spoilers drawn as an overlay | Only `TextEdit` gives selectable formatted text; `StyledText` exists only for `Text` (§8.4) |
| App id | `app.kourier.Kourier` (desktop file, Wayland `app_id`, D-Bus name); QML URI `app.kourier` | A placeholder (user decision, 2026-10-08): the project is separate from everything else of the user's, has no site or domain of its own yet, and names no domain or account of his. It is replaced when the project has its final name and a home; until then the id is one string per crate (`APP_ID`) plus the three file names in `packaging/` |
| Text ranges | UTF-8 byte offsets in the core; conversion only at the Signal edge (UTF-16) and the Qt edge (QString UTF-16) | Rust-native, and WhatsApp/Threema produce ranges by parsing markup in Rust (§3.6) |
| Storage Service in M1 | Read-only. Roaming chat state (archive, pin, mute, block) is shown, not changed. A Kourier-local "mute on this computer" exists. A 1:1 chat the phone has not accepted (`whitelisted = false`) cannot be started from Kourier | Writing without unknown-field preservation can clobber the phone's state (R:signal-desktop-send §5.5); sending without profile sharing would show us as an unnamed request (§6.7) |
| Group send in M1 | Sender key + Group Send Endorsements via `send_multi_recipient_message`, with per-member 1:1 fan-out as the fallback | Access-key group sends are being retired (`SD/ts/util/sendToGroup.preload.ts:301`) |
| Capabilities advertised | Link URL `capabilities=` (`backup5` on a fresh link, as Desktop's, so the phone asks whether to transfer the history, §14.2 item 14; nothing on a relink; never `nopni`); device capabilities `spqr: true`, `attachmentBackfill: true`, `usernameChangeSyncMessage: true`, `optionalPhoneNumber: false` | Desktop's values: the server derives the account's capabilities from its devices, so a device that claims less could lower them for the whole account (§6.5 step 5, §13 phase K). `attachmentBackfill`: Kourier sends backfill requests (§6.11 "Asking the phone") and takes the answers only from the primary |
| Linker | rustc's self-contained rust-lld via `-fuse-ld=lld` in `.cargo/config.toml`; no extra package | Any `-fuse-ld` flag stops cxx-qt from forcing gold; the Qt spike and S0 link and run this way [V] (§2.4) |

---

## 1. Goals, non-goals, quality targets, M1 scope

### 1.1 Goals

1. A Signal client for KDE Plasma 6 on Wayland that starts fast, uses little memory and behaves as
   a native app: correct `app_id`, XDG activation, KDE notifications with inline reply, tray,
   Breeze styling, spell checking, Wayland input methods.
2. Interoperate correctly as a Signal *linked device*: follow Signal Desktop's behavior for
   linking, receive, send and sync (R:signal-desktop-receive §9 obligations, R:signal-desktop-send §12).
   Desktop is the reference for the protocol, not for its limits (user decision, 2026-10-06): a
   limit is kept only when the server or other clients enforce it or when it protects interop,
   privacy or security. A limit that is only Desktop's UI choice is not copied; where one message
   cannot carry what the user asked for, Kourier adapts (splits, converts, queues) instead of
   refusing. Example: Desktop's composer accepts one non-image file per message; Kourier takes any
   number of files of any kind in one send and splits them into the messages every Signal client
   shows in full (§3.8 `file_packing`; the audit of the other limits is in §13 phase H).
3. A protocol-agnostic core, so Threema and WhatsApp backends plug in with no core redesign (§7).
4. Crash safety: no lost or duplicated messages after a crash, power loss, suspend or network change.
5. Hermetic tests: the Signal backend is tested against `@signalapp/mock-server`, never against the
   user's real account by default.

### 1.2 Non-goals (M1 and probably later)

- Primary-device registration (phone number + SMS). Kourier only links as a secondary device.
- Calls (RingRTC), stories, payments, polls UI,
  message backups, sticker pack management, usernames management, CDSI phone-number discovery.
  (Link-and-sync history transfer, `backup5`, stood in this list for the first milestone; the
  user moved it into the release gate on 2026-10-08: §14.2 item 14.)
- Group management (create, invite, admin actions), profile editing.
- Windows/macOS builds, Flatpak packaging (both stay possible; nothing in the design is
  Linux-only except the desktop-integration crate).
- Cross-network person merging (an optional later `person` table, §3.3).

### 1.3 Quality targets

Measured on this machine (6 cores, 31 GB, Plasma 6.7.5 Wayland), release build, one Signal account
with about 200 conversations and 50,000 messages. Signal Desktop is the baseline to beat; its
renderer, GPU and main processes typically total several hundred MB [I: not measured here].

The budgets are in force as the user confirmed them (when, and what they replaced, is told
after the table), and `cargo xtask perf` enforces them. The "Measured" column is phase I's
result (S16, 2026-10-07, `cargo xtask perf`, §13 phase I): release build, offscreen software
renderer, window 1200x800, a reference profile of 200 conversations and 50,000 messages (8 %
with media) on the fake network, three full runs; a range is the three runs' medians. No Signal
account is in these numbers: the Signal backend's memory and its receive path inside the app
are not measured yet. The two memory rows also carry phase J's numbers, which come from two
quick runs (10 s of idle instead of 300 s, on a busy machine; §13 phase J) and are named as
such.

| Target | Budget | Measured (phase I) | How measured |
|---|---|---|---|
| Cold start to first frame | ≤ 400 ms on a warm page cache | 339 to 366 ms, slowest single start 377 ms (10 starts per run, warm page cache) | `KOURIER_TRACE_STARTUP=1` prints `kourier-trace:` lines; `startup.first_frame` is `main` → first `frameSwapped` |
| Start with Kourier's own files out of the page cache | ≤ 600 ms | 443 to 478 ms (3 starts per run) | same trace; before each start the binary, the profile and Kourier's cache directory are written out and evicted from the page cache, the system's libraries stay cached. A start after boot with cold system libraries stays a manual check |
| First frame to populated conversation list (from `core.db`, before any network) | ≤ 50 ms | 0 ms: the first frame shows the rows (30 starts) | same trace (`startup.list_populated`) |
| Open a conversation (first 50 rows visible) | ≤ 80 ms | 50 to 56 ms, slowest 67 ms (21 conversations per run; the query alone 4 ms) | trace span `timeline.open` |
| Idle memory, window shown | anonymous memory (`Pss_Anon`) ≤ 100 MB on any session; PSS ≤ 150 MB on a session whose programs map the same Qt files, ≤ 170 MB when no library page is shared | 157 to 158 MB PSS with no library page shared (below), so the 170 MB limit applies. Anonymous 87 MB, file-backed 70 MB. Phase J, two quick runs on a session where other processes carried 44 % of the Qt 6 libraries (less than half, so still the 170 MB limit): 138.9 and 138.5 MB PSS, anonymous 88.8 and 88.3 MB, file-backed 50 MB | `/proc/<pid>/smaps_rollup` after 5 min idle, on a release build with cargo feature `dumpable` (release otherwise sets `PR_SET_DUMPABLE 0`, which hides `smaps_rollup` from the same user, §9.4) or read as root. `cargo xtask perf` picks the PSS limit from the same process's `smaps`: the lower one when other processes carry half or more of the resident pages of the Qt 6 libraries (their PSS is at most half of their RSS), the higher one when they carry less; the row says which and why |
| Idle memory, hidden to tray | anonymous ≤ 70 MB, to be tightened to 60 MB once the SQLite caches are released with the window and a measurement shows it (60 MB is not enforced yet); PSS ≤ 110 MB with shared libraries, ≤ 145 MB unshared (QML root window released after 60 s hidden, §8.2) | 130 to 133 MB PSS with no library page shared, so the 145 MB limit applies. Anonymous 60 to 63 MB. Phase J, with the core's caches released with the window (`UiCommand::ReleaseMemory`, §8.2 step 6), two quick runs, 10 s after the release, the 145 MB limit again: 109.6 and 108.2 MB PSS, anonymous 59.5 and 58.0 MB. That is too little under 60 MB, from too short a run, to enforce the 60 MB limit: a full run after a login decides | same |
| Idle CPU | On the fake network: ≤ 0.2 wakeups/s over 5 min for all threads, no 100 s part above 0.5/s, process CPU below 50 ms per 5 min. With a Signal connection the target stays in words until it is measured: no periodic wakeups besides the chat keepalive (30 s) and timers that have work | Fake network (no keepalive): 0.04 wakeups/s shown, 0.01 hidden over 300 s, busiest 100 s part 0.09, process CPU under 10 ms. Signal mock with the 30 s keepalive: 0.42 wakeups/s over 90 s for all 9 threads (`kourier-e2e` `signal_idle`, run by hand) | `cargo xtask perf` counts the voluntary context switches of all threads (`/proc/<pid>/task/*/status`) over the idle time and over each of its 100 s parts, and reads the process's CPU time from `/proc/<pid>/stat`: three rows per state. A run that idles for less than 5 min (`--quick`) shows them for information. On a real connection: `powertop` / `perf stat` over 10 min |
| Receive → notification | ≤ 150 ms from the envelope at stage 1 to the accepted notification, and ≤ 50 ms for `recv.commit` → `notify.shown` (outside catch-up) | 37 ms median (30 to 43) on the fake network, envelope at stage 1 → notification accepted by the server; of that `recv.commit` → `notify.shown` 9 to 11 ms | marks `recv.stored` (`kourier-signal`, stage 1) and `recv.commit` (core) on the `tracing` target `kourier::perf`, `notify.shown` in the app: `KOURIER_TRACE_STARTUP=1 KOURIER_LOG=warn,kourier::perf=trace` |
| Catch-up ingest | ≥ 3,000 messages/s for the release build on the Signal mock, from stage 1 to core commit (5,000 queued 1:1 and group texts); the mock test's own assert of 500 stays for debug builds. History with media on the fake network: ≥ 800 messages/s, for information only | 9,219 to 9,766 messages/s, slowest run 8,812 (release build on the Signal mock; the test queues 5,000 1:1 texts of one sender and times connect → all committed). Debug build: 2,450 (S7). History with 8 % media on the fake network: 1,294 to 1,375 messages/s | mock test `kourier-e2e` `five_thousand_queued_messages_ingest_at_500_per_second`; the core's mark `recv.catchup`. Fake network: the seeding of the reference profile by `cargo xtask perf` |
| Binary size | ≤ 60 MiB for the default release profile | 52.2 MiB | `target/release/kourier` as built with default features. The budget is there so that debug information coming back fails the table (260 MB once) |
| Timeline scroll | no frame > 16 ms while flicking through image-heavy history | Not measured on the GPU. Software renderer, offscreen: median 15.9 to 16.1 ms, slowest frame 17.7 to 18.6 ms | on the Plasma session: `QT_LOGGING_RULES="qt.scenegraph.time.renderloop.debug=true"` with `KOURIER_LOG=qt=debug` (Qt 6 has no `QSG_RENDER_TIMING`) |
| Wayland | runs on `wayland` QPA with no XWayland; `app_id = app.kourier.Kourier`; window raise via `xdg-activation-v1`; fractional scaling crisp; `text-input-v3` IME; DnD and clipboard images; portal or KDE file dialogs | not run (manual) | manual checklist in §10.6 |

Phase I missed both memory budgets the design had then (150 MB PSS shown, 80 MB PSS hidden),
and the two halves of the number behave differently:

- The file-backed half (70 MB) was at its upper bound when it was measured. The Qt packages of
  this machine had been updated since the login, so the running desktop's Qt programs mapped
  the replaced (deleted) library files and Kourier shared no library page with any of them.
  Beside three stand-in Kirigami programs on the same Qt files, the same fresh start measured
  97 MB PSS instead of 133 MB (file-backed 36 instead of 72 MB). After a login the expected
  numbers are about 122 MB shown and 98 MB hidden: estimates, to be confirmed with
  `cargo xtask perf --no-mock`, whose report says how much of the Qt 6 libraries other
  processes map and carry. A session can also lie in between. Later on 2026-10-07 a few
  programs ran on the current Qt files (a browser's helpers among them) while the desktop was
  still on the replaced ones: a start of the measuring build on the fixture then had 48 MB
  file-backed, other processes mapped 95 % of its Qt6Core and carried 46 % of its Qt 6
  libraries. That is why the tool decides by what the others carry of all Qt libraries, and
  holds such a session to the higher limit.
- The anonymous half is Kourier's own and stable: 86.8 to 86.9 MB shown, 60.1 to 62.7 MB
  hidden. With the binary's own pages (17 MB) the hidden state is already at 79 MB before any
  library page, so the old hidden budget cannot be met while the QML engine is kept. The one
  located contributor that phase I left alone was the SQLite page caches of the core's
  connections: while hidden, the allocator arenas of the other threads held 19.7 MB, the
  largest the size of one connection's 8 MiB cache. The core has `UiCommand::ReleaseMemory`
  for them, and since phase J the UI sends it when the hidden window is let go (§8.2
  step 6). Phase I expected 10 to 15 MB from it, an estimate from the arena sizes that was
  too high. Measured in phase J: the heap in use at the trim is 26 MB instead of 31 MB, and
  the hidden anonymous memory is 59.5 and 58.0 MB in two quick runs against 60.1 to 62.7 MB,
  so between 0.6 and 4.7 MB less. Most of the freed pages stay resident as fragments of
  arenas the allocator trim cannot hand back (48 MB are free inside the arenas after the
  trim). In the same runs the hidden PSS read 109.6 and 108.2 MB, against 130 to 133 MB in
  phase I. That drop is the session, not the release: other processes carried 44 % of the
  Qt 6 libraries, in phase I none (file-backed 50 MB instead of 70 MB).

The budgets in the table were confirmed by the user on 2026-10-07 ("lower our memory efficiency
expectation a bit"); they are the proposals phase I made from these numbers. Before that the
design had, besides the two memory budgets named above, 300 ms to the populated list, 100 ms
to open a conversation, 300 ms to the notification and 500 messages/s of catch-up; idle CPU
was stated in words only, and the start with cold files, the span from `recv.commit` to the
notification and the binary size had no budget. Memory was relaxed for two reasons. The hidden
budget cannot be met while the QML engine is kept (second point above), so it is withdrawn.
And PSS, the number a system monitor shows, moved by 36 MB with what else ran on the session,
while `Pss_Anon` was stable to 0.1 MB shown and 2.6 MB hidden across the runs: so the
anonymous memory is judged on every session, and the PSS against one limit for a session that
shares its Qt files and another for a session that does not. The other budgets were tightened
to what was measured, with headroom; the start on a warm page cache kept its 400 ms because
the numbers do not support a tighter one.

Memory baseline [V, S0 `51ee9db`]: the release build of the empty Kirigami shell (QApplication,
`org.kde.desktop` style, offscreen QPA, 4 s idle) has 26.9 MB PSS (19.0 MB anon, 7.8 MB file) and
82.4 MB RSS. That baseline does not carry over to the app: its file-backed part was 7.8 MB,
and the phase I build maps 209 shared objects at start (Qt6 34.5 MB, KDE Frameworks 7.5 MB,
QML plugins 4.5 MB, the binary's own pages 16.7 MB when nothing is shared). S16 was to tighten
both budgets from the baselines; the measurements say the opposite for memory (the confirmed
budgets above).

Design rules that serve these budgets (as built since phase I, §13):

- The UI renders from `core.db` immediately. Backends connect after the first frame.
- The QML engine creates only the window and the conversation list with it. The chat, welcome,
  link and settings pages, the emoji popup and the reaction picker are created on first use;
  the list gets its model only once its page is in the page stack and has a size (a `ListView`
  without a height creates a delegate for every row). The message menu and the who-reacted
  list are still created with the chat page. QML is AOT-compiled by `qmlcachegen` (cxx-qt does
  this, R:qt-kirigami §2.1).
- No message caches in memory beyond the visible timeline window (≤ 500 rows). SQLite page cache
  is bounded per connection (`PRAGMA cache_size = -8000`, 8 MiB).
- Images are decoded at display size (`sourceSize`) by an async image provider (§8.6). Bundled
  icons are `Kirigami.Icon`s, not QML `Image`s: the first `Image` with a file or `qrc:` URL
  makes Qt list its image formats, which loads every installed image plugin.
- When hidden to tray for more than 60 s, the root QML window is destroyed (engine kept) and
  the core is asked to release the page caches and prepared statements of its connections
  (`UiCommand::ReleaseMemory`). `malloc_trim(0)` runs only once the window object is really
  gone, after a `gc()`, and again 2 s later, because the core frees its caches on its own
  threads and is not waited for; another trim runs 5 s after the first frame (§8.2 step 6).

### 1.4 M1 scope (user decision, 2026-10-05)

In: link as a secondary device via QR; sync contacts and groups; send/receive text in 1:1 and
groups; delivery and read receipts; typing indicators; image and file attachments; desktop
notifications; encrypted local database. M1 also includes everything needed for *correctness* even
if not shown in full: edits, remote deletes, reactions (display + receive), quotes (display),
disappearing timers, profile-key updates, end-session, view-once placeholders, retry receipts,
sync transcripts and read syncs (R:signal-desktop-send §7).

Out: calls, stories, payments, Storage Service writes, group management,
`nopni` (E164-less accounts), CDSI. (History transfer was out for the first milestone and
is in the release gate now: §14.2 item 14.)

---

## 2. Layering and Cargo workspace

### 2.1 Layering diagram

```
                ┌──────────────────────────── kourier (bin) ─────────────────────────────┐
                │ args → single instance → kourier_app::start() → kourier_qt::run()       │
                └──────────────┬───────────────────────────────────────┬──────────────────┘
 UI (Qt thread)                │                                       │
   ┌───────────────────────────▼──────────┐        ┌───────────────────▼───────────────┐
   │ kourier-qt  (cxx-qt bridges, QML,    │───────▶│ kourier-viewmodel (Qt-free rows,  │
   │  image-provider C++ shim)            │        │  diffs → ListOp, formatting)      │
   └───────────────────┬──────────────────┘        └───────────────────┬───────────────┘
                       │ AppHandle (commands / UiEvents / media reads) │ facade DTOs only
 Facade                ▼                                               ▼
   ┌──────────────────────────────────────────────────────────────────────────────────┐
   │ kourier-app: runtime, paths, secrets, network registry (cargo features),          │
   │              desktop services; re-exports kourier_core::facade                    │
   └───────┬──────────────────────┬─────────────────────────┬─────────────────┬───────┘
 Core      │                      │                         │                 │
   ┌───────▼──────────┐   ┌───────▼─────────┐   ┌───────────▼──────────┐ ┌────▼─────────┐
   │ kourier-core     │   │ kourier-desktop │   │ backends (one crate  │ │ kourier-fake │
   │ core.db, ingest, │   │ zbus notif,     │   │ each): kourier-signal│ │ scripted     │
   │ outbox, modifiers│   │ tray, secrets,  │   │ (M1), -threema,      │ │ FakeNetwork  │
   │ timers, policy,  │   │ single instance,│   │ -whatsapp (later)    │ └────┬─────────┘
   │ supervisor,facade│   │ logind/NM watch │   └───────────┬──────────┘      │
   └───────┬──────────┘   └─────────────────┘               │                 │
 Contract  │ uses Network / Command / Envelope               │ implements      │
   ┌───────▼─────────────────────────────────────────────────▼─────────────────▼───────┐
   │ kourier-backend-api (Network, AccountContext, Command, Envelope, Ack, Clock)       │
   │ kourier-model       (ids, content, features, receipts, errors; pure serde data)   │
   └───────┬───────────────────────────────────────────────────────────────────────────┘
 Infra     ▼
   ┌──────────────────┐ ┌───────────────────┐ ┌─────────────────────┐
   │ kourier-db       │ │ kourier-blob      │ │ kourier-media       │
   │ SQLCipher open,  │ │ encrypted files,  │ │ decode/resize/EXIF/ │
   │ migrations       │ │ tmpfs media cache │ │ blurhash/thumbnails │
   └──────────────────┘ └───────────────────┘ └─────────────────────┘
```

### 2.2 Crates

| Crate | Responsibility | May depend on (internal) | Key external deps | Must never depend on |
|---|---|---|---|---|
| `kourier-model` | The data contract: ids, content model, features, receipts, states, errors, login steps. Pure data, `serde`. | — | serde, serde_json, thiserror, uuid | tokio, rusqlite, protocol crates, Qt |
| `kourier-backend-api` | The boundary: `Network` trait, `AccountContext`, `AccountHandle`, `Command`, `BackendEvent`, `Envelope`, `Ack`, `Clock` | model | tokio (`sync`), tokio-util (`CancellationToken`) | core, db, protocol crates, Qt |
| `kourier-db` | Open SQLCipher files with a raw key, pragmas, forward-only migration runner, `schema_version`, writer-thread helper, synchronous `zone` API, the generic `core_outbox` table (opaque versioned bytes) and its forwarder loop used by every backend (§5.6) | — | rusqlite 0.38, libsqlite3-sys 0.36 (patched), zeroize | anything domain-specific |
| `kourier-blob` | Encrypted file format (streaming AEAD), atomic writes, `$XDG_RUNTIME_DIR/kourier/media` plaintext cache with cleanup | — | aes-gcm (STREAM), rand, zeroize | domain crates |
| `kourier-media` | Image decode/resize/re-encode, EXIF strip, blurhash, thumbnails; used by core (import) and backends (per-network transcoding) | — | image, blurhash | Qt, protocol crates |
| `kourier-core` | `core.db` schema and queries, account registry and supervisor, ingest, outbox, pending modifiers, disappearing-message sweeper, typing timers, notification *policy*, read-state logic, `facade` (`CoreHandle`, `UiCommand`, `UiEvent`, query DTOs) | model, backend-api, db, blob, media | tokio, tracing | any backend crate, zbus, Qt |
| `kourier-fake` | `FakeNetwork`: scripted backend for core, UI and screenshot tests (login flows, duplicates, permutations, slow acks, crash points), with its own `core_outbox`; contract-conformance suite (§10.2) | model, backend-api, db | tokio | core, Qt |
| `kourier-signal` | Signal backend (§6) | model, backend-api, db, blob, media | libsignal-* (git tag v0.104.0), prost 0.14, reqwest 0.13 (rustls + ring), tokio | core, app, desktop, Qt, other backends |
| `kourier-threema` / `kourier-whatsapp` | Later backends (§7) | same as signal | libthreema (vendored) / whatsapp-rust (git rev) | same as signal |
| `kourier-desktop` | zbus notification client (inline reply, activation token), `org.freedesktop.Application` single-instance server, tray (ksni), Secret Service (oo7), autostart file, logind `PrepareForSleep` and NetworkManager watchers | — | zbus 5, oo7 0.6, ksni (git) | core, Qt, protocol crates |
| `kourier-app` | Composition root: tokio runtime, XDG paths, master key and per-account key wraps (§5.2), registry of compiled-in networks (cargo features `signal`, `fake`), wiring of core + desktop; `AppHandle` and the media handle table for the UI (§8.6); the walk and the .zip of a folder given to the composer (`folders`, §8.3) | core, backend-api, model, desktop, blob, signal (feature), fake (feature) | tokio, tracing-subscriber, zip | Qt |
| `kourier-viewmodel` | Qt-free UI projections: conversation rows, message rows with grouping flags, diff engine → `ListOp`, timestamp and preview formatting, typing text | core (`facade` module only), model | — | Qt, backend crates, `kourier_core` internals |
| `kourier-qt` | The only cxx-qt crate: `App` singleton, list models, controllers, `image://kourier` C++ shim, QClipboard and badge bindings; all QML under `crates/kourier-qt/qml/` | app, viewmodel, model | cxx-qt 0.10, cxx-qt-lib (`qt_full`), cxx-qt-lib-extras | core internals, db, backend crates |
| `kourier-coexist` (test-only) | One process proving libsignal-net BoringSSL, SQLCipher (RustCrypto) and Qt's OpenSSL TLS backend coexist; `nm -D` clean (§5.3, S1) | db | libsignal-net-infra, rustls, tokio, cxx-qt-build (C++ shim for QtNetwork) | everything else |
| `kourier` (bin) | `main`: CLI, single-instance check, start app, run Qt, orderly shutdown | qt, app | anyhow, clap | — |
| `xtask` | `cargo xtask ci`, `deps-check`, `symbols-check`, `shots`, `demo`, `perf`, `install`, `uninstall` | — | anyhow, serde_json (it reads `cargo metadata`'s JSON), libc (`posix_fadvise` in `perf`) | — |

Non-cargo directories: `tests/mock-harness/` (Node harness for the Signal mock server, §10.3),
`packaging/` (`.desktop`, icon, AppStream metainfo; the autostart entry is written by the app),
`docs/`.

### 2.3 Enforcing the layering

- `cargo xtask deps-check` checks every internal edge from `cargo metadata` against a whitelist
  in `xtask/src/main.rs` that mirrors the table above (self-tested in S0 [V]), so for example
  `kourier-core → kourier-signal` or `kourier-qt → kourier-db` fails.
- It also greps `kourier-viewmodel` and `kourier-qt` sources for `kourier_core::` paths other than
  `kourier_core::facade`.
- `crates/kourier-qt/clippy.toml` sets `disallowed-methods` for `tokio::runtime::Runtime::block_on`,
  `tokio::runtime::Handle::block_on` and `futures::executor::block_on`: the Qt thread never blocks.
- Protocol types (prost structs, libsignal types, whatsapp-rust types) never appear in public items
  of a backend crate. A backend's only public items are its `Network` implementation and config.

### 2.4 Workspace files

`kourier/Cargo.toml` is in the repo since S0 (workspace `members = ["crates/*", "xtask"]`,
`edition = "2024"`, `rust-version = "1.95"` because oo7 0.6 needs it; libsignal needs 1.93.1; we
build with 1.99; release profile `lto = "thin"`, `codegen-units = 4`, `debug = "line-tables-only"`).
What later steps add, and why:

```toml
[workspace.dependencies]
# libsignal: ONE tag for every crate, bumped together (§6.15). The spike built this set [V]:
# libsignal-core, libsignal-protocol, signal-crypto, zkgroup, libsignal-account-keys,
# libsignal-net-infra, libsignal-net, libsignal-net-chat, libsignal-net-grpc,
# signal-media (mp4san for outgoing video), each:
libsignal-protocol = { git = "https://github.com/signalapp/libsignal", tag = "v0.104.0" }
prost    = "0.14"
rustls   = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
reqwest  = { version = "0.13", default-features = false, features = ["rustls-no-provider", "http2", "stream"] }
rusqlite = "0.38"   # 0.38.0 requires libsqlite3-sys ^0.36.0 [V crates.io]
libsqlite3-sys = { version = "0.36", features = ["bundled-sqlcipher-custom-crypto"] }
zbus = "5"
oo7  = { version = "0.6", default-features = false, features = ["tokio", "native_crypto"] }

[patch.crates-io]
# Mirrors LS/Cargo.toml:241-245: guarantees a single BoringSSL (`links = "boringssl"`).
boring     = { git = "https://github.com/signalapp/boring", tag = "signal-v5.2.0" }
boring-sys = { git = "https://github.com/signalapp/boring", tag = "signal-v5.2.0" }
# SQLCipher with a RustCrypto crypto provider (no OpenSSL) + atexit fix; gurk ships this rev
# (refs/gurk-rs/Cargo.toml:133-137) [V].
libsqlite3-sys = { git = "https://github.com/boxdot/rusqlite", rev = "a2f393913d9994645c94810498bef07b2183b3dc" }

[profile.dev.package.sqlcipher-crypto-provider]
opt-level = 3          # DB decryption is slow in debug otherwise (gurk does the same)
[profile.dev.package.sha2]
opt-level = 3
```

`kourier/.cargo/config.toml` (both entries fix traps verified on this machine):

```toml
[env]
QMAKE = "/usr/bin/qmake6"   # /opt/qt/6.9.3/gcc_64/bin is first on PATH; system Kirigami needs system Qt 6.11

[target.x86_64-unknown-linux-gnu]
# Any -fuse-ld flag stops cxx-qt's build script from forcing GNU gold. rustc's self-contained
# rust-lld is found first; -B adds the system lld-18 as a fallback.
rustflags = ["-C", "link-arg=-fuse-ld=lld", "-C", "link-arg=-B/usr/lib/llvm-18/bin"]
```

- [V] The Qt spike (`spikes/qt-kirigami`) built and rendered a Kirigami window with the
  `org.kde.desktop` style (`capture.png`, 2026-10-05 19:06). Without a linker flag its build
  scripts emitted `cargo::rustc-link-arg=-fuse-ld=gold`; cxx-qt's docs tie old linkers to missing
  `cxx_qt_init_*` symbols in multi-crate setups (R:qt-kirigami §2.6). With `-fuse-ld=lld` gold is
  not forced, rustc's self-contained rust-lld (LLD 23.1.1) links, and the spike and S0 link and
  run [V]. No apt package is needed (mold is not used).
- [V] S0 (`51ee9db`) uses cxx-qt's multi-crate pattern: `kourier-qt` sets `links = "kourier_qt"`
  and calls `.build().export()` in its build script; the `kourier` bin calls
  `cxx_qt::init_crate!(kourier_qt)`. The release binary is 23.8 MB with line-table debuginfo.
- Never link with `-rdynamic`: BoringSSL's OpenSSL-named symbols must stay out of the dynamic
  symbol table, because Qt's TLS plugin loads the system `libssl.so.3` (§5.3).
  `cargo xtask symbols-check` fails if `nm -D --defined-only target/release/kourier` lists any
  `SSL_`, `EVP_` or `CRYPTO_` symbol; the S0 binary has none [V] (it does not link libsignal yet,
  so the check becomes meaningful in S1/S2).
- [V] The libsignal spike (`spikes/libsignal-dep`) built libsignal v0.104.0 (protocol, core,
  zkgroup, net, net-chat, signal-crypto) including BoringSSL with the installed protoc 3.21.12 and
  cmake 4.4.3. Its lock file has 373 packages.
- `rust-toolchain.toml`: `channel = "1.99"`, components `clippy, rustfmt, rust-src`. Bumped on
  purpose. Never run cargo with cwd inside `refs/libsignal`: its `rust-toolchain` (1.98.1) makes
  rustup install another toolchain (R:qt-kirigami §2.6 item 9; the copy research installed by
  accident was removed on 2026-10-05).
- `.gitignore` additions: `.qmlls.ini` (absolute paths), `tests/mock-harness/node_modules/`,
  `/target/shots/`.

### 2.5 Process and thread model

One process. Threads:

```
Qt main thread ─ QApplication::exec(), all QObjects, models, QML
  │  invokables → AppHandle::send(UiCommand)        (non-blocking try_send / unbounded)
  │  ◀── UiEvent pump: one queued closure at a time, 8 ms budget (CxxQtThread::queue)
  │
tokio multi-thread runtime "kourier-rt" (worker_threads = min(4, cores)): async glue and network I/O
  ├─ core: account supervisor, envelope intake per account, outbox scheduler, timers, notification policy
  ├─ desktop: zbus connection, tray, logind/NetworkManager watchers
  └─ network I/O for every backend (libsignal-net reader/writer tasks, reqwest, CDN streams)
  No blocking rusqlite call ever runs on a runtime worker.
core.db writer: dedicated std::thread with a job scheduler (UI commands and live envelopes first,
  then history chunks; round-robin across accounts, §5.4) + 3 read-only connections (2 for UI
  queries, 1 for media-handle misses), used from the blocking pool
Per Signal account: "signal-<uuid8>" std::thread
  └─ current_thread runtime + LocalSet: protocol actor, owns backend.db + libsignal stores (!Send),
     stage-2 lanes and the core_outbox forwarder (§6.6)
Per WhatsApp account (later): "wa-<uuid8>" std::thread owning backend.db; whatsapp-rust store calls
  reach it over a channel (§7.1)
Blocking pool (tokio spawn_blocking): image decode, thumbnails, blurhash, attachment crypto
Qt image-provider thread pool: resolves media handles from an in-memory table, decrypts from
  kourier-blob (§8.6)
```

---

## 3. Core domain model

All types live in `kourier-model` unless noted. Everything persisted is serde with named enum
variants (never by index) and `#[serde(default)]` for evolution.

### 3.1 Identifiers: local vs network

| Kind | Type | Scope | Notes |
|---|---|---|---|
| Local ids | `AccountId(i64)`, `ConversationId(i64)`, `MessageId(i64)`, `ContactId(i64)`, `AttachmentId(i64)` | `core.db` rowids | Used by the core and the UI only. Never sent to a backend except as an opaque correlation handle (`LocalMsgId` = `MessageId`; `AttachmentId` for downloads; `CmdId` for durable commands) |
| Account uuid | `AccountUuid(Uuid)` | stable forever | Names `accounts/<uuid>/`; recorded in `key.wrap`, so it survives a `core.db` rebuild (§5.2) |
| Network kind | `NetworkKind::{Signal, WhatsApp, Threema, Fake}` | — | Stored as TEXT |
| User id | `UserId(Arc<str>)` | per account | Opaque, backend-defined, versioned grammar. Signal: `aci:<uuid>`, `pni:<uuid>`, `e164:+<digits>`. WhatsApp: `lid:<n>`, `pn:<n>`. Threema: `ECHOECHO`. The core never parses it |
| Conversation key | `ConvKey::{Direct(UserId), Group(Arc<str>), Broadcast(Arc<str>), Channel(Arc<str>)}` | per account | Signal group: `gv2:<base64url group id>` (the id, never the master key). Threema group: creator id + u64. `Broadcast` and `Channel` are reserved for Threema distribution lists and WhatsApp newsletters |
| Message id | `NetMsgId { author: Option<UserId>, local: Arc<str> }` | see matching scope | `author: None` = our own account (whichever own id the network used). Stored as `net_author` (`''` = own) and `net_local`. Signal: sender ACI + sent timestamp in ms (author + timestamp is Signal's message identity, `SD/ts/messages/handleDataMessage.preload.ts:123-145`). WhatsApp: sender + stanza id. Threema: sender + u64 hex. The core never parses `local` |
| Matching scope | `NetworkDescriptor.msg_id_scope: AccountAuthor \| Conversation` | — | `AccountAuthor` (Signal): key `(account, author, local)`, unique account-wide, as Desktop dedups by sender + `sent_at` (R:signal-desktop-receive §5). `Conversation` (WhatsApp, Threema): key `(conversation, local)`; the author is compared only when both sides carry one, after alias resolution |
| Message reference | `MsgTarget { conv: Option<ConvKey>, author: TargetAuthor, local: Arc<str>, alternates: Vec<Arc<str>> }`, `TargetAuthor::{Own, User(UserId), Unknown}` | — | `conv: None` when the network does not say (Signal receipts and read syncs carry only author + timestamp); the core then looks the key up account-wide (`AccountAuthor` scope only). `Unknown`: the reference has no sender (Threema reactions, quotes and receipts carry only the u64, `TD/packages/libthreema-wasm/libs/libthreema/threema-protocols/src/csp-e2e.proto:605-607`). `alternates` = other `local` values of the same message; none are known today. PNI/ACI and PN/LID aliasing goes through `identity_alias`, never through ids |
| Backend data | `BackendBlob { v: u16, bytes: Vec<u8> }` | — | Opaque bytes the core stores verbatim: attachment pointers, raw payloads, per-row backend metadata |

Rules:
- Id grammars are versioned (prefix) and frozen once data exists (mautrix's warning in
  `networkid/bridgeid.go`, R:multi-protocol-prior-art §2.1).
- Own ids never enter stored message ids. A WhatsApp own message allocated while the chat is
  PN-addressed still matches its LID-addressed echo and receipts (`WA/src/request.rs:268-280`
  derives the id from the device PN; R:whatsapp §8 req 1).
- A phone number is never a required field anywhere.

### 3.2 Account

```rust
pub struct Account {
    pub id: AccountId, pub uuid: AccountUuid, pub network: NetworkKind,
    pub state: AccountState,            // Linking | Active | LoggedOut{reason} | Removing | Unlinking
    pub own_ids: Vec<UserId>,           // Signal: [aci, pni?]; WA: [lid, pn]; Threema: [id]
    pub own_device: Option<u32>,
    pub display_name: Option<String>,   // own profile name, for the account switcher
    pub settings: AccountSettings,      // synced from the network (read receipts, typing, link previews…)
    pub local: AccountLocalPrefs,       // Kourier-only: notifications on/off, auto-download policy
}
```

### 3.3 Contacts, aliases, members, persons

- `Contact` (per account, per `UserId`): profile name, system/nickname, avatar (blob ref),
  `trust: TrustLevel::{Unverified, ServerVerified, Verified}` plus `identity_fingerprint`,
  `blocked`, `hidden` (Threema group-only/deleted contacts, R:threema §7 req 3),
  `unregistered_at`, `identifiers: Vec<Identifier>` (phone, username: for display and search only),
  `meta: Option<BackendBlob>`.
- `identity_alias(account, alias UserId → canonical UserId)`. A backend event
  `IdentityMerged { from, into }` rewrites `from` to `into` in contacts, members, `net_author`,
  reactions, receipts, `send_state` and `pending_modifiers` (a resulting key conflict merges the
  two rows as in step 1 below), then either re-keys the direct
  conversation of `from` or, if `into` already has one, runs `merge_conversation`. The alias row
  stays, so references that still name `from` resolve. Signal uses it for PNI→ACI (after a verified
  `PniSignatureMessage`) and E164→ACI (Storage Service `ContactRecord`s carry ACI, PNI and E164;
  Desktop's `maybeMergeContacts` in `SD/ts/ConversationController.preload.ts`); WhatsApp for
  PN→LID (R:whatsapp §8 req 1: "must collapse without losing messages").
  A merge costs what `from` owns, never the account's size (link feedback L2, 2026-10-12): the
  first Storage Service sync after a link tells one merge per contact with a phone number, and
  each merge used to walk all of the account's messages about nine times (~2 µs per contact and
  message, minutes in one write job for a big account, and the link page waited for it). Now
  (`ingest/merge.rs`, core.db schema 14): every lookup of the alias's rows starts from an index
  of its own rows (`messages(account_id, net_author)`, `messages(account_id, deleted_by)`
  partial, `message_revisions(account_id, net_author)`, `reactions(user_id)`,
  `receipts(user_id)`, `send_state(user_id)`, and for the keys that live 45 days or until
  their message comes, `expired_messages(account_id, net_author, net_local)` and
  `(account_id, conv_key, net_local)`, `pending_modifiers(account_id, target_author)` and
  `(account_id, conv_key)`, and `identity_alias(account_id, user_id)` for the repointing of
  older aliases), joins are
  `CROSS JOIN`s from the user's rows (core.db has no planner statistics, so an `a = ? OR
  b = ?` is two `EXISTS`: given the OR the planner took the index of `account_id` alone),
  deletes are `EXISTS`-scoped; one `ALIAS_OWNS` query first, and an alias that owns nothing
  only gets its alias row and the contacts change flag; the parked receipts whose sender may
  have become a recipient are looked at once per transaction, after all its merges
  (`Changes::receipts_to_retry`, consumed in `Apply::finish`), not once per merge; the
  writer's statement cache holds 128 statements. The two keys of `expired_messages` end in
  `net_local` because `expiry::remembered`, run for every new message, names the author (or
  the conversation) and the local id: with two columns they tied with the older
  `expired_messages_by_key` and the planner, taking the newer index on a tie, walked all of
  that author's expired keys per message (40 ms a message at 45,000 keys).
  The supervisor cuts a live segment after 64 merges (`supervisor::MERGES_PER_JOB`), so a
  user's action waits for one job of merges, not for all of them. The plan test is
  `ingest::merge::tests::a_merge_looks_only_at_the_rows_of_the_alias`, the budget test
  `kourier-core/tests/merge_cost.rs` (20,000 messages, 45 days of expired keys, 5,000 parked
  modifiers, 303 merges: 25 ms in debug, 7.7 s without the indexes; and 300 merges look at
  each of 40 parked receipts once).
- `merge_conversation(from, into)` (core, one `core.db` transaction):
  1. Move messages. On a key conflict (§3.1) keep the older row (smaller `sort_key`), merge state
     into it monotonically (out/read state, revisions, reactions, receipts, `send_state`), delete
     the other.
  2. Move `pending_modifiers` (rewrite `conv_key`) and members (union).
  3. Recompute `unread`, `mentions`, `last_activity`; `read_up_to` = max; draft = newest non-empty;
     local mute = later; roaming fields of `into` win.
  4. Delete `from`; keep the alias row.
  5. Emit `UiEvent::ConversationMerged { from, into }`: an open ChatPage switches to `into`, and
     notifications of `from` are replaced.
  The S3 permutation property tests run the merge before, between and after the messages of both
  conversations and require the same final state.
- `Member` (per conversation): `UserId`, `role: Member | Admin | Pending | Requesting`, and a
  contact row is optional.
- `Person` (later, optional): links (account, user) pairs across networks for display only.

### 3.4 Conversations

```rust
pub struct Conversation {
    pub id: ConversationId, pub account: AccountId, pub key: ConvKey,
    pub kind: ConvKind,                         // Direct | Group | Broadcast | Channel
    pub is_self: bool,                          // note to self
    pub title: Option<String>, pub avatar: Option<BlobRef>,
    pub members: Vec<Member>,                   // empty for Direct
    // Roaming state: changed ONLY by backend events (source of truth = network)
    pub muted_until: Option<i64>, pub archived: bool, pub pinned_order: Option<u32>,
    pub blocked: bool, pub request_pending: bool, pub marked_unread: bool, pub timer: Option<Duration>,
    pub parent: Option<ConversationId>,         // reserved: WhatsApp community
    pub features: ConversationFeatures,         // §3.8, hashed for UI caching
    // Core-local state
    pub local_muted_until: Option<i64>, pub draft: Option<Text>,
    pub unread: u32, pub mentions: u32, pub read_up_to: Option<i64 /* sort_key */>,
    pub last_activity: i64 /* sort_key of newest visible message */,
}
```

A chat our account deleted on another own device is hidden, not removed: the core keeps the row
with its title, members and features (`conversations.hidden`, §5.4) and leaves it out of the
conversation list until a message is stored or queued in it again.

### 3.5 Messages and content

```rust
pub struct Message {
    pub id: MessageId, pub conversation: ConversationId,
    pub net_id: NetMsgId,                       // net_id.author is the author; None = own account
    pub origin: Origin,
    pub body: Body, pub reply_to: Option<ReplyTo>,
    pub sent_at: i64, pub server_at: Option<i64>,
    pub received_at: i64, pub arrival: Option<u64>,          // from the backend's stage 1 (§3.9)
    pub sort_key: i64,
    pub revision: u16, pub edited_at: Option<i64>,           // edits: §3.5.3
    pub deleted: Option<Deletion>,                           // tombstone: ForEveryone{by} | ForMe
    pub out_state: Option<OutgoingState>,                    // own messages
    pub read_state: Option<ReadState>,                       // incoming: Unread → Read → Viewed
    pub expiry: Option<Expiry>,                              // timer + started_at (AfterRead/AfterSend)
    pub view_once: bool, pub history: bool,                  // history = bulk import, never notifies
    pub linked: Option<MessageId>,                           // per-member copy of a broadcast send
    pub meta: Option<BackendBlob>, pub raw: Option<BackendBlob>,
}

pub enum Origin {
    Remote { author: UserId, device: Option<u32> },          // device clears typing from that device
    OwnThisDevice,
    OwnOtherDevice { device: Option<u32> },
}

pub enum Body {
    Text(Text),
    Media { attachments: Vec<Attachment>, caption: Option<Text> },
    Sticker(Attachment),
    Contact(ContactCard),                 // shared contact (render name + identifiers)
    Location(Location),                   // reserved (Threema, WhatsApp)
    Poll(Poll),                           // reserved
    System(SystemEvent),
    Placeholder(Placeholder),             // Undecryptable{resendable}, ViewOnceOpened, Expired
    Unsupported { kind_hint: String, summary: Option<String> },   // raw kept in Message.raw
}

pub struct Attachment {
    pub id: Option<AttachmentId>, pub kind: AttachmentKind,  // Image|Video|Audio|Voice|Gif|File
    pub mime: String, pub size: u64, pub file_name: Option<String>,
    pub dims: Option<(u32, u32)>, pub duration_ms: Option<u32>, pub blurhash: Option<String>,
    pub waveform: Option<Vec<u8>>, pub caption: Option<Text>,
    pub state: AttachmentState,           // Remote | Downloading{done,total} | Local | Failed{retryable} | Expired
    pub remote: Option<BackendBlob>,      // Signal AttachmentPointer bytes, WA directPath+mediaKey, Threema blob ref
    pub local: Option<BlobRef>,           // kourier-blob file + key (core-owned)
    pub thumb: Option<BlobRef>,
    pub long_text: bool,                  // the rest of the message's text, not a file (§5.8)
    pub net_ids: Vec<String>,             // opaque names the network has for it (Signal: client UUID,
                                          // digest), so AttachmentDeleted can name one; never shown
}

pub struct ReplyTo { pub target: MsgTarget, pub author: Option<UserId>, pub snapshot: Option<QuoteSnapshot> }

pub enum SystemEvent {
    MembersAdded { by: Option<UserId>, users: Vec<UserId> }, MembersRemoved { by: Option<UserId>, users: Vec<UserId> },
    MemberLeft { user: UserId }, TitleChanged { by: Option<UserId>, title: String }, AvatarChanged { by: Option<UserId> },
    DescriptionChanged { by: Option<UserId> }, TimerChanged { by: Option<UserId>, timer: Option<Duration> },
    IdentityKeyChanged { user: UserId }, SessionRefreshed { user: UserId }, GroupCreated { by: Option<UserId> },
    MessageRequestAccepted, MissedCall { from: UserId, video: bool }, Joined, NumberChanged { user: UserId },
}
```

#### 3.5.1 Reactions, receipts, read markers

- `reactions(message, user, emoji, at)`; `ConversationFeatures.reactions_per_user` is 1 for Signal
  and WhatsApp (a new reaction replaces the old one) and unbounded for Threema (a set of emoji per
  sender, R:threema §4.11).
- `receipts(message, user, kind ∈ {Delivered, Read, Viewed}, at)` per member, plus the aggregated
  `OutgoingState` on the message.
- `ReadMarker { conv, up_to }`: own read position from another own device (Signal
  `SyncMessage.read`, WhatsApp `MarkChatAsReadUpdate`, Threema reflected read).

#### 3.5.2 Deletes

Tombstones keep the row and its message key, so later modifiers still resolve; the body is cleared
and attachments are deleted from disk. Delete-for-me (Signal `deleteForMe` sync) is a tombstone
with `ForMe`. A delete for me of a whole chat or of one attachment deletes rows and files for
good. Made on another own device it arrives as `ConversationCleared` or `AttachmentDeleted`
(§4.9). Made in Kourier (`UiCommand::ClearConversation`, `DeleteConversation`,
`DeleteAttachment`) it is applied at once in one transaction that also stores the durable
command telling the other devices (`Command::ClearConversation`, `Command::DeleteAttachment`,
§4.8 "Durable commands"). Both directions follow the same rule for what stays. A delete for
everyone with `by: None` on another member's message is our own admin delete from another device;
it applies when the features have `admin_delete` and one of our own ids is an admin there.

#### 3.5.3 Edits

`message_revisions(message, revision, net_author, net_local, body, edited_at)`. Signal edits are
new DataMessages with a new timestamp whose `targetSentTimestamp` is the *previous revision's*
timestamp (`SD/ts/util/editHelpers.std.ts:20-50`), so every revision's key is indexed and resolves
to the same message. The latest revision is copied into `messages.body`. An edit of a message
with attachments is a new caption: the attachment rows and files stay. The target of an own edit
is chosen when the edit is sent, not when it was typed: the revision right before its own that
still exists (an earlier edit the network refused is rolled back and never reached anyone).
Only a text, a caption (where every file kind of the message takes one; a caption can always be
removed) and an undecryptable placeholder take an incoming edit. A sticker, contact card,
location, poll, notice, view-once content and unsupported content never do: the core drops such
an edit without a trace. The versions of a message are read with `Query::Revisions`.

### 3.6 Text, formatting ranges, mentions

```rust
pub struct Text { pub body: String, pub ranges: Vec<TextRange> }
pub struct TextRange { pub start: u32, pub len: u32, pub kind: RangeKind }   // UTF-8 byte offsets
pub enum RangeKind { Bold, Italic, Strike, Spoiler, Mono, Mention(UserId), MentionAll, Link(String) }
```

- A mention is one `U+FFFC` in `body` covered by a `Mention` range. Signal uses the same
  placeholder and UTF-16 ranges (`SD/protos/SignalService.proto:1018-1019`,
  `SD/ts/types/BodyRange.std.ts:582`) [V via R:multi-protocol-prior-art §4.3].
- Decision: UTF-8 offsets. One conversion utility (`kourier_model::text::Utf16Map`) is used at the
  Signal edge (proto ↔ model) and at the Qt edge (composer selection, rendering); it is
  property-tested with astral-plane emoji and ZWJ sequences. Ranges are validated on ingest
  (char boundaries, inside the body); invalid ranges are dropped, not fatal.
- WhatsApp and Threema backends parse their inline markup (`*bold*`, `> quote #<id>`,
  `@[ECHOECHO]`) into this model and render it back on send.

### 3.7 Outgoing and incoming states

```rust
pub enum OutgoingState {                       // monotonic; never moves backwards
    Queued, Sending { progress: Option<f32> }, Sent { server_at: Option<i64> },
    Delivered, Read, Viewed,
    Failed { reason: FailReason, retryable: bool },
    PartiallyFailed { failed: Vec<UserId> },   // groups: some recipients failed
}
pub enum ReadState { Unread, Read, Viewed }
```

Per recipient the core stores the send outcome (`send_state`: `Pending | Sent | Failed{retryable}`,
written from `TransmitOutcome`) and the receipts; the aggregate is recomputed (all delivered →
Delivered, some failed for good → `PartiallyFailed`). A retry resends only to members whose
`send_state` is not `Sent` (§4.8). Signal's ordering `Pending < Sent < Delivered < Read < Viewed` is
the model (`SD/ts/messages/MessageSendState.std.ts`).

### 3.8 Capabilities

Three tiers:

1. `NetworkDescriptor` (static, per network): display name, icon name, login flows, identifier
   kinds accepted for "start chat", default `history: HistorySource::{None, PushedOnLink, OnDemand}`
   (a conversation may override it, e.g. WhatsApp newsletters are fetched per conversation),
   `msg_id_scope: AccountAuthor | Conversation` (§3.1), `typing: TypingPolicy { refresh, stop_after }`
   (Signal 10 s / 3 s, `SD/ts/models/conversations.preload.ts:1270-1320`),
   `risk_notice: Option<RiskNotice>` (shown before login; §7.4).
2. `AccountCaps` (dynamic, backend event `AccountCaps`): limits from server config (Signal
   `global.attachments.maxBytes`, text attachment limit), feature flags.
3. `ConversationFeatures` (per conversation, sent with `ConversationUpsert`, hashed):

```rust
pub enum Support { Rejected = -2, Dropped = -1, Unsupported = 0, Partial = 1, Full = 2 } // bridgev2 levels
pub struct ConversationFeatures {
    pub max_text_bytes: Option<u32>, pub formatting: Vec<(FormatKind, Support)>,
    pub files: Vec<(AttachmentKind, FileLimits { max_size: u64, caption: bool })>,
    pub file_packing: FilePacking {              // what ONE message carries so every client shows it all
        albums: Vec<FileAlbum {                  // files that share a message, by kind and by MIME type:
            kinds: BTreeSet<AttachmentKind>,     //   a kind as a whole,
            types: BTreeSet<String>,             //   single formats of a kind that otherwise travels alone,
            except: BTreeSet<String>,            //   formats of a listed kind that travel alone (wins over types)
            max: u16 }>,
        reply_needs_text: bool },                // a message with files cannot quote (Signal: false)
    pub hard_to_save: BTreeSet<String>,          // file-name extensions some client of the network shows but
                                                 // will not save; a hint for the composer, never a refusal
    pub reply: Support, pub edit: Option<Window { max_age_secs: Option<u32>, max_count: Option<u16> }>,
    pub delete_for_everyone: Option<Window>, pub delete_for_me: bool,
    pub edit_receive: Option<Window>, pub delete_receive: Option<Window>,   // windows for incoming edits/deletes
    pub admin_delete: bool,                      // group admins may delete others' messages
    pub reactions_per_user: Option<u8>,          // None = unlimited set
    pub typing: bool, pub read_receipts: bool, pub delivery_receipts: bool,
    pub disappearing: Vec<(DisappearKind /* AfterRead | AfterSend */, Vec<u32>)>,   // the offered list (menu)
    pub disappearing_custom: BTreeMap<DisappearKind, TimerRange { min_secs, max_secs, step_secs }>,
                                                 // every other timer the network takes (custom time)
    pub history: HistorySource,                  // from the descriptor unless the conversation differs
    pub group_actions: GroupActions,             // add/remove/rename/avatar/leave/disband (all false in M1)
    pub cannot_send: Option<CannotSend>,         // NotMember | AdminsOnly | Blocked | RequestPending
                                                 // | Unregistered | NotAccepted | StillLoading
                                                 // | InvitationPending | JoinRequestPending
                                                 // | Other(String)
}
```

The core validates commands against features *before* queuing (for example, edit window); the
backend re-validates and may still return `Unsupported`. The same check functions
(`kourier-core/src/actions.rs`) fill the per-row action flags the UI builds its menus from, so a
flag and its command never disagree. Incoming edits and deletes are checked by
the core, because only the core sees the target row: author (or admin with `admin_delete`), that
the target is a message of the conversation the modifier states (§4.9), that its body can be
edited at all (§3.5.3), and the receive window, measured as an absolute distance from the
original's `server_at` (its `sent_at` when it has none) to the modifier's time. The backend
checks only the wire form of an edit.

`file_packing` states what one message of the network carries so that every client of that
network shows all of it; it is never a limit on what the user may pick. The pure function
`ConversationFeatures::plan(files: &[PlanFile { kind, mime }], caption: Option<&Text>, reply) ->
FilePlan { lead_text, caption_messages, messages, reply_unsendable }` splits a selection of any
size and mix: neighbours of the same album share a message until it is full, every other file
is a message of its own, the user's order is kept. A file joins an album when the album lists
its kind and does not except its MIME type, or lists its MIME type; the core reads kind and
type from the file's first bytes (`kourier_media::sniff_mime`), never from its name.
`caption_messages` counts the texts a caption becomes when it goes first (also a caption above
`max_text_bytes`, cut by `Text::split`). The core's send and the composer's preview call the
same function. The default (no albums) sends every file alone. The older `plan_files(kinds,
caption: bool, reply)` is a wrapper for callers without types or text: on a network whose
albums name only types (Signal) it plans every file alone. A file the user sends "as original
file" (`SendFiles.as_file`) is a generic `File` with its true MIME type: no backend re-encodes,
scales or strips a `File`, and it packs like any other generic file.

Signal (phase I; sources in §13 phases H and I): the album is an allow-list of formats that
Desktop, Android and iOS all draw in an album, `image/jpeg`, `image/png`, `image/gif`,
`image/webp`, `image/bmp` and `video/mp4`, at most min(`android.attachments.maxCount` − 1, 32)
of them, 31 without a remote config. HEIC, HEIF, AVIF, QuickTime, WebM, Matroska, any format
not listed, audio, voice notes and other files travel one per message.

Disappearing timers: `SetConversationTimer` takes a value of the offered list or of a range in
`disappearing_custom` (`takes_timer`); "off" wherever the chat has any timer (`has_timers`).
Signal states `AfterRead` 1 s to 2,147,483,647 s in whole seconds for 1:1 chats and nothing for
groups. The upper end is the largest value Android reads as a positive number (it takes the
`uint32` wire field as a signed integer, and anything larger shows there as "off"); four weeks
is only where the pickers of the official clients end, and no client refuses or shortens a
longer timer it receives (§13 phase J).

Signal values follow what the server and the official clients enforce, not Desktop's composer:
edit ≤ 24 h (or the server's delete window when that is shorter) and ≤ 100 edits when sending
(no receiver enforces Desktop's 10; iOS drops edits after the hundredth) but ≤ 48 h when
receiving; delete-for-everyone ≤
`global.normalDeleteMaxAgeInSeconds` (default 1 day) when sending, that plus 1 day when receiving
(R:signal-desktop-receive §9 item 26); Note to Self has no edit window and no edit count, sending
or receiving, and no delete for everyone; max file: the largest plaintext whose padded, encrypted
upload fits `global.attachments.maxBytes` (102,224,512 bytes for the default 100 MiB); a body
over 2 KiB is sent as a long-text attachment (`SD/ts/util/longAttachment.std.ts:7-8`).
Which limits Kourier keeps, which it dropped, and who enforces each is the table in §13 phase H.
`NotAccepted` ("Start this chat on your phone") is §6.7's rule for 1:1 chats without profile
sharing.

### 3.9 Ordering and timestamps

- Stored: `sent_at` (sender's claim, ms), `server_at` (server timestamp if any), `received_at`
  (local wall clock when the backend's stage 1 stored the envelope, carried in `IncomingMessage`),
  `arrival` (the backend's stage-1 counter), `sort_key` (core-assigned).
- Decision: `sort_key` for live messages = `max(last_sort_key_of_conversation + 1,
  received_at_ms << 10)`, ties broken by `arrival` (a monotonic arrival order, like Signal Desktop's
  `received_at`, `SD/ts/util/incrementMessageCounter.preload.ts`), so clock skew between senders
  never reorders live traffic, and parked lanes, batching and replays keep wire arrival order.
  History batches get `sort_key = sent_at_ms << 10` (+ tie-breaker), so they interleave
  chronologically with live data.
- Placeholder replacement: when a message arrives whose key matches an `Undecryptable`
  placeholder (Signal retry flow, R:signal-desktop-receive §4.4), the row keeps its `sort_key` and
  gets the real body.
- Timeline order: `ORDER BY sort_key DESC` per conversation.

---

## 4. Core ↔ backend interface

### 4.1 Principles

1. **Message passing, not async trait objects.** Each account is an actor that owns its runtime.
   The only trait object is the small synchronous `Network` factory.
2. **Serializable boundary.** Everything crossing is `Send + 'static`, serde-able data. Files cross
   as paths, never as bytes. A backend could later run in a child process (whatsmeow sidecar
   fallback, R:whatsapp §6) without core changes.
3. **At-least-once with idempotent ingest.** The backend acks the server only after its own durable
   commit; the core ingest is idempotent on the message key of §3.1.
4. **The network is the source of truth for roaming state.** The core changes roaming fields
   (archive, mute, pin, block, membership, contact names) only from backend events. User actions on
   roaming state go to the backend as commands; the backend confirms by emitting the event after
   its network transaction (needed for Threema transactions and WhatsApp app-state, R:threema §7
   req 14, R:whatsapp §8 req 8). Core-local state (drafts, local mute, UI prefs) is committed
   locally.
5. **Backends own their protocol view.** Sessions, keys, group secrets, nonce stores and
   decrypted-but-unprocessed envelopes live in the backend's `backend.db`; the core never reads it.
6. **Transactional outbox (contract rule for every backend).** Every `backend.db` transaction that
   changes protocol or mirrored state (ratchets, identities, group state, contact and storage
   records, app-state cursors, nonces, jobs) inserts, *in the same transaction*, the
   `BackendEvent`s it implies into `core_outbox`. Durable events reach the core only from there:
   one forwarder per account ships rows in `seq` order and deletes them after the core's `Ack`.
   Network fetches run before such a transaction, never inside it. Ephemeral events (typing,
   progress, login steps) bypass the outbox with `ack: None`. Without this rule a crash between
   the backend commit and the core commit loses everything derived by diffing against backend
   state: group system messages, identity-change notices, Storage Service merges, Threema
   contact/group syncs, WhatsApp app-state mutations (§5.6, failpoints §10.4).

### 4.2 Contract sketch (`kourier-backend-api`)

Not compiled; fixes shapes and semantics.

```rust
use kourier_model::*;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// One per network, process-wide, synchronous, dyn-compatible.
pub trait Network: Send + Sync + 'static {
    fn kind(&self) -> NetworkKind;
    fn descriptor(&self) -> &NetworkDescriptor;
    /// Pure function. Returns only the network-local part of an own message id plus `sent_at`;
    /// the stored author of own messages is always `None`. The core persists the result in the
    /// outbox BEFORE the first transmit, so retries reuse it. `now_ms` is strictly increasing per
    /// account (core ensures).
    fn allocate_message_id(&self, own: &[UserId], conv: &ConvKey, now_ms: u64) -> (Arc<str> /*local*/, i64 /*sent_at*/);
    /// Pure function, default empty. The names attachment `index` of own message `local` will have
    /// on the network, in the string form of `Attachment::net_ids` (Signal: `uuid:<hex>` of the
    /// client UUID derived from both). The core stores them with the queued message and echoes
    /// them in the request, so `AttachmentDeleted` from another device finds attachments this
    /// device sent, and a delete made here can name them.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String>;
    /// Spawns the per-account actor (Signal: own OS thread + current_thread runtime + LocalSet).
    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError>;
}

/// Built by kourier-app from cargo features; injected into the core (core never names a backend).
pub struct NetworkRegistry(Vec<Arc<dyn Network>>);

pub struct AccountContext {
    pub account: AccountId,
    pub uuid: AccountUuid,
    pub start: StartMode,                    // Fresh (login first) | Resume | Rebuild (core.db was rebuilt: re-emit mirrored state)
    pub backend_db: PathBuf,                 // accounts/<uuid>/backend.db (exclusive to this backend)
    pub db_key: DbKey,                       // Zeroizing<[u8; 32]>, unwrapped from accounts/<uuid>/key.wrap (§5.2)
    pub blob_dir: PathBuf,                   // accounts/<uuid>/blobs (via kourier-blob only)
    pub net_rt: tokio::runtime::Handle,      // shared multi-thread runtime for network I/O
    pub commands: mpsc::Receiver<Command>,   // bounded (64)
    pub events: mpsc::Sender<Envelope>,      // bounded (8): the core_outbox forwarder + ephemeral events
    pub conn: watch::Sender<ConnState>,
    pub ui: watch::Receiver<UiPresence>,     // window focused, visible conversation, idle (WA presence)
    pub local_prefs: watch::Receiver<LocalPrefsView>,   // Kourier-local prefs only; synced settings come as events
    pub cancel: CancellationToken,           // child of the app token
    pub clock: Arc<dyn Clock>,
    pub config: BackendConfig,               // env (production | staging | local test), proxy, log level,
                                             // initial_sync: LinkOnly | ContactsAndStorage (§6.5)
}

pub struct AccountHandle {
    pub commands: mpsc::Sender<Command>,
    pub conn: watch::Receiver<ConnState>,
    pub exited: oneshot::Receiver<Exit>,     // supervisor awaits this
}
pub enum Exit { Shutdown, LoggedOut { reason: String }, Failed(BackendError), Panicked(String) }

pub trait Clock: Send + Sync { fn now_ms(&self) -> u64; }

// ---------- backend → core ----------
pub struct Envelope { pub events: Vec<BackendEvent>, pub ack: Option<Ack> } // one core transaction
pub struct Ack(pub oneshot::Sender<Result<(), PersistError>>);              // completed after commit
// Every Envelope with durable events comes from the core_outbox forwarder and carries an Ack;
// `ack: None` only for ephemeral events (Login, Typing, *Progress).

pub enum BackendEvent {
    Login(LoginStep),
    AccountReady { own_ids: Vec<UserId>, device: Option<u32>, display_name: Option<String>,
                   number: Option<String> /* own number (Signal E.164); None leaves what the core has */ },
    AccountSettings(AccountSettingsPatch),            // read receipts, typing, link previews, sealed-sender indicators…
    AccountCaps(AccountCaps),
    ContactUpsert(ContactPatch),                      // Option<Option<T>> = unchanged | clear | set
    IdentityMerged { from: UserId, into: UserId },
    ConversationUpsert(ConversationPatch),            // title, avatar, members (full|delta), features, timer,
                                                      // timer_version (the core drops the timer of an older patch),
                                                      // muted_until, archived, pinned_order, blocked, request_pending,
                                                      // marked_unread, unread_hint (history), parent
    PinnedConversations(Vec<ConvKey>),               // full ordered list
    ConversationRemoved { conv: ConvKey },
    ConversationCleared { conv: ConvKey, up_to: Vec<MsgTarget>, remove: bool, at: i64 },
                                                      // own delete for me of a chat's messages on another
                                                      // own device (§4.9); remove = the chat itself too.
                                                      // May arrive before the messages it names: the core parks it
    AttachmentDeleted { target: MsgTarget, net_ids: Vec<String>, at: i64 },   // one attachment, same origin;
                                                      // also matches attachments this device sent (attachment_net_ids)
    Message(IncomingMessage),
    HistoryBatch { conv: ConvKey, messages: Vec<IncomingMessage> /* ≤ 500 */, has_more: bool, progress: Option<f32> },
    Edited { target: MsgTarget, by: Option<UserId> /* None = own */, new_local: Option<Arc<str>>, body: Body, edited_at: i64,
             keeps_quote: Option<bool> /* Some(false): the edit carries no quote, the message loses its
                                          quote for good (Signal); None: the network does not say */ },
                                                      // media message: body = the new caption (Text, empty = removed,
                                                      // or Media{caption}); attachments in the edit are ignored.
                                                      // body is only ever Text or Media; the core decides whether
                                                      // the target can be edited (§3.5.3)
    Deleted { target: MsgTarget, by: Option<UserId>, for_everyone: bool, at: i64 },
    Reaction { target: MsgTarget, by: Option<UserId>, emoji: String, remove: bool, at: i64 },
    Receipts { targets: Vec<MsgTarget>, by: UserId, kind: ReceiptKind, at: i64 },
                                                      // counts only from someone the message was sent to (§4.9);
                                                      // a backend that knows the conversation sets targets[i].conv
    ReadMarker { conv: Option<ConvKey>, up_to: MsgTarget },       // own read elsewhere
    OwnSendState { local: Option<LocalMsgId>, target: MsgTarget, per_recipient: Vec<(UserId, RecipientState)> },
    CommandPersisted { cmd: CmdId },                 // a durable command is in the backend's own job table (§4.8)
    Typing { conv: ConvKey, by: UserId, device: Option<u32>, on: bool, ttl: Duration },  // ephemeral
    AttachmentProgress { local: LocalMsgId, index: u16, done: u64, total: u64 },
    AttachmentDone { attachment: AttachmentId, request: Option<DownloadId> /* echoed; an answer to
                     another request is ignored */, result: Result<AttachmentMeta, BackendError> },
    TransmitProgress { local: LocalMsgId, progress: f32 /* upload share */,
                       phase: Option<SendPhase> /* Uploading | RefreshingGroup |
                       SettingUpKeys{done,of} | RepairingDevices | Sending{done,of} |
                       SyncingOwnDevices; ephemeral, a group send reports it at most
                       every 250 ms per phase (link feedback L6) */ },
    Notice(BackendNotice),                           // safety-number change pending, …; ChallengeRequired{url,
                                                     // retry_after}: a job met a challenge (§4.8 step 3)
}

pub struct IncomingMessage {
    pub conv: ConvKey, pub local: Arc<str>,
    pub origin: Origin,                              // NetMsgId = (Remote author | None for own, local)
    pub body: Body, pub reply_to: Option<ReplyTo>, pub sent_at: i64, pub server_at: Option<i64>,
    pub received_at: i64, pub arrival: Option<u64>,  // from the backend's stage 1 (§3.9)
    pub expiry: Option<Expiry>, pub view_once: bool, pub mentions_me: bool,
    pub silent: bool,                                // never notify (history, …)
    pub catch_up: bool,                              // arrived before the catch-up boundary: summarized (§4.5)
    pub initial_out_state: Option<OutgoingState>,    // own messages in history imports
    pub read: bool,                                  // already read elsewhere (history imports)
    pub edits: Vec<HistoryEdit>,                     // history only: later versions' ids and times, oldest first
    pub reactions: Vec<(Option<UserId>, String, i64)>, // history imports
    pub linked_local: Option<LocalMsgId>,            // per-member copy of a broadcast send (Threema lists)
    pub raw: Option<BackendBlob>, pub meta: Option<BackendBlob>,
}

// ---------- core → backend ----------
pub enum Command {
    Connect, Disconnect, Shutdown,
    Logout,                                          // unlink this device on the network, then Exit::LoggedOut
    NetworkChanged, Resumed,                         // NetworkManager / logind hints
    StartLogin { flow: String, relink: Option<Relink> },   // "qr", later "pair-code", "id-backup"
    SubmitLoginInput { txn: u64, values: Vec<(String, String)> },
    CancelLogin,
    Transmit { local: LocalMsgId, req: OutgoingRequest, reply: oneshot::Sender<TransmitOutcome> },
    // Durable: the core keeps a pending_commands row until CommandPersisted{cmd} (§4.8).
    SendReceipts { cmd: CmdId, conv: ConvKey, targets: Vec<MsgTarget>, kind: ReceiptKind },  // read / viewed
    SyncReadMarker { cmd: CmdId, conv: ConvKey, up_to: MsgTarget, read: Vec<MsgTarget> },
    // Durable too: deletes for me made on this device. The core has deleted locally already; the
    // backend tells the account's other devices and confirms with CommandPersisted, also when the
    // network has nothing to tell.
    ClearConversation { cmd: CmdId, conv: ConvKey,
                        newest: Vec<MsgTarget> /* ≤ CLEAR_ANCHORS (5) newest person-sent messages the chat
                        held, newest first */, newest_non_expiring: Vec<MsgTarget> /* the same without
                        messages that have a timer */, remove: bool /* the chat itself was deleted */ },
    DeleteAttachment { cmd: CmdId, conv: ConvKey, target: MsgTarget, index: u16 /* position before the
                       removal */, net_ids: Vec<String> /* every name the attachment had; never empty */ },
    SetTyping { conv: ConvKey, on: bool },           // ephemeral
    DownloadAttachment { attachment: AttachmentId, request: DownloadId /* attachment_downloads.id,
                         never reused */, remote: BackendBlob, dest: BlobTarget },
    // Roaming changes; the backend confirms with events after its network transaction (principle 4).
    ChangeConversation { conv: ConvKey, change: ConversationChange,
                         id: Option<AllocatedId> /* SetTimer only: an id from the core's own-message allocator
                         for the update message and the timeline row the backend writes (note below) */,
                         reply: oneshot::Sender<Result<(), BackendError>> },
    ChangeContact { user: UserId, change: ContactChange, reply: oneshot::Sender<Result<(), BackendError>> },
    ChangeAccountSettings { patch: AccountSettingsPatch, reply: oneshot::Sender<Result<(), BackendError>> },
    ChangeProfile { patch: ProfilePatch, reply: oneshot::Sender<Result<(), BackendError>> },
    ConfirmIdentity { user: UserId },                // user accepted a safety-number change
    SubmitChallenge { token: Option<String> /* ErrorKind::Challenge.token of the paused attempt */,
                      answer: String /* what the browser handed back: solution token or redirect link */,
                      reply: oneshot::Sender<Result<(), BackendError>> },  // Ok: the core lifts its pause (§4.8)
    FetchHistory { conv: ConvKey, before: Option<MsgTarget>, limit: u16 },       // only if HistorySource::OnDemand
    Resolve { id: Identifier, reply: oneshot::Sender<Result<UserId, BackendError>> },
    ListOwnDevices { reply: oneshot::Sender<Result<Vec<OwnDevice>, BackendError>> },
    RefreshContact { user: UserId },
}
pub struct Relink { pub account: AccountId, pub expect_own: Vec<UserId> }   // refuse another account (§4.3)

pub struct OutgoingRequest {
    pub conv: ConvKey,
    pub local: Arc<str>, pub sent_at: i64,           // allocated once at enqueue; every retry reuses them
    pub kind: OutgoingKind,
    pub only: Option<Vec<UserId>>,                   // None = all recipients; Some = resend to these members only
    pub is_recipient_update: bool,                   // follow-up send to more recipients (Signal sync transcript flag)
}
pub enum OutgoingKind {
    Message { body: Body, reply_to: Option<ReplyTo>, expiry: Option<Duration> },
    Edit { target: MsgTarget, body: Body },
    Delete { target: MsgTarget, for_everyone: bool },
    React { target: MsgTarget, emoji: String, remove: bool },
}
pub enum TransmitOutcome {
    Sent { server_at: Option<i64>, per_recipient: Vec<(UserId, RecipientState)> },
    Partial { delivered: Vec<(UserId, RecipientState)>, failed: Vec<(UserId, BackendError)> },
    Failed(BackendError),
}
```

Notes:
- `Option<Option<T>>` in patches = (unchanged | clear | set). `MemberList { full: bool, .. }`
  separates a snapshot from a delta (bridgev2 semantics, R:multi-protocol-prior-art §2.1).
- `BlobTarget { path, key }` is created by the core; the backend streams plaintext into
  `kourier_blob::Writer::create(target)` and never sees where else keys live.
- `UiPresence` and `local_prefs` are `watch` channels because they are state, not events.
  Network-synced account settings (read receipts, typing, link previews) are owned by the backend
  and reported with `AccountSettings`; changing them is `ChangeAccountSettings`.
- `ChangeConversation` covers archive, pin, mute, block, accept message request, leave;
  `ChangeContact` (names, block, verify), `ChangeAccountSettings` and `ChangeProfile` exist because
  Threema routes every shared-state mutation through a D2M transaction (R:threema §7 req 14) and
  WhatsApp through app state. In M1 the Signal backend answers `Unsupported` for all four (Storage
  Service read-only, §6.9), except `ChangeConversation{SetTimer}` in a 1:1 chat (§6.13); the UI
  hides these actions through `features`.
- Timer rows: the backend writes the `TimerChanged` timeline row for own changes and for incoming
  ones (`by: None` = our own account); the core never writes one and changes a conversation's
  timer only from the patch. For an own `SetTimer` the backend uses `id` as the id of the update
  message it sends and as `local`/`sent_at` of the row, answers `reply`, and confirms with
  `ConversationUpsert{timer, timer_version}` + `Message{origin: OwnThisDevice, body:
  System(TimerChanged{by: None, timer})}`. An id the backend made up itself could be that of a
  message waiting in the core's outbox, and the row would be merged as that message's echo.
- Broadcast sends (Threema distribution lists, later): one `Transmit` on the `Broadcast`
  conversation; the backend sends 1:1 copies and emits, through `core_outbox`, one
  `Message{origin: OwnThisDevice, linked_local}` per member DM, so the one-message,
  one-conversation outbox model holds (R:threema §7 req 22).

### 4.3 Account lifecycle and supervision

1. **Add account**: the UI picks a network; if `descriptor.risk_notice` is set, the UI shows it and
   requires explicit consent. The core creates an `accounts` row (`state = Linking`, new uuid),
   creates `accounts/<uuid>/` (mode 0700) with a fresh `key.wrap` and account secret (§5.2), calls
   `Network::spawn(start = Fresh)` and sends `StartLogin{flow, relink: None}`.
2. **Login** streams `Login(LoginStep)` events (§4.4). On `Done` the backend emits `AccountReady`;
   the core sets `state = Active`. On `Failed` or `CancelLogin` the core shuts the actor down and
   deletes the account secret, the directory and the row. A `CancelLogin` and every login the
   core turns away (a relink as another account, a login that failed) is followed by
   `Command::Logout` first (`Supervisor::log_out`, at most `Timeouts::logout`), so a link that
   finished just then does not leave a device on the server; a backend without a device simply
   exits on it. A backend never aborts a login that may already have registered something: the
   Signal backend stops it, waits for the answer of a link request that is on its way and
   removes the device the server linked (§6.5).
3. **Startup**: for each `Active` account, unwrap `db_key`, spawn with `start = Resume`, send
   `Connect` after the first UI frame.
4. **Supervisor**: awaits `exited`. `Panicked`/`Failed` → restart with capped exponential backoff
   (1 s … 5 min, jitter); after 5 failures in 10 min the account shows an error with "Restart".
   Two failures are not retried, because a restart changes nothing about them, and neither
   deletes anything: `Failed` with `ErrorKind::Unsupported` (the backend refuses the account as
   it was started, §9.2) halts the account at once with the backend's detail; a `key.wrap` that
   is there with its secret and does not open (`VaultError::WrongKey`: a master key that is not
   this profile's) halts it too. Only a `key.wrap` or a secret that is truly missing counts as
   the rest of an unlink or a removal (step 5, and the reconciliation of step 8).
5. **Unlinked** (Signal `DeviceDelinked`, 401/403 on the auth socket, mock `unlink`; WhatsApp
   `LoggedOut`): the backend stops all network work, finishes stage 2 for rows already in its inbox
   without network steps (≤ 10 s), lets the forwarder drain `core_outbox` (≤ 30 s), and exits with
   `Exit::LoggedOut`. The core then deletes the account secret, `backend.db` (+ `-wal`, `-shm`) and
   `key.wrap`: credentials, identity keys, sessions, prekeys, sender keys, MSL and inbox are gone
   (Signal obligation 8: "stop, wipe credentials and keys, offer to relink",
   R:signal-desktop-receive §9). `state = LoggedOut`; core conversations and messages stay
   readable; a banner offers "Link again".
6. **Relink** ("Link again"): new `key.wrap`, account secret and empty `backend.db` for the *same*
   `AccountId`; `spawn(start = Fresh)` and `StartLogin{flow, relink: Some(Relink{account,
   expect_own: own_ids})}`. The backend checks the provisioned identity *before* finishing the link
   (Signal: ACI from the provisioning message, before `PUT /v1/devices/link`); a different account
   fails with "This is a different Signal account; add it as a new account". On `Done` the account
   is `Active` again and conversations continue under the same `ConvKey`s. Desktop does the same:
   same ACI keeps messages, another account wipes them
   (`SD/ts/textsecure/AccountManager.preload.ts:1113-1149`). Reusing an old `backend.db` (old
   device id, registration ids, sessions) is never allowed. The core checks the own ids of
   `AccountReady` again wherever that event reaches it, and until it has accepted them it takes
   nothing else in from the actor and sends it no `Transmit`, roaming change, typing, durable
   command or reconnect hint. While it links it also gets no `DownloadAttachment` (the download
   is stored and sent after the link), no `ConfirmIdentity` (only the core's own pause is
   released; a fresh link has an empty identity store) and no `SubmitChallenge` (the command
   fails with "the account is not running"). What waited in the outbox goes out only as the
   account that wrote it.
   One identity is one account of a profile, checked twice. FIRST LINE, in the backend, before a
   device exists: `Command::StartLogin` carries `taken`, the primary own ids (with whether that
   account is logged out) of the profile's other accounts of the network that are not
   `Removing`; the Signal backend compares the ACI of the phone's provisioning message with
   them and, on a match, ends the login with `ErrorKind::AlreadyAdded { logged_out }` BEFORE
   `PUT /v1/devices/link` (the phone never gets a second device; the mock harness counts the
   link requests, `sendStats.linkRequests`). SECOND LINE, in the core, for a backend that
   cannot know early (or ignores `taken`, as the fake does): an `AccountReady` of an account
   that is not yet `Active` (a fresh link or a relink) whose primary own id is the primary own
   id of another such account is refused in the ingest transaction that would take it
   (`CoreError::AlreadyAdded`, so two links ending together cannot both pass): the login fails
   with the same kind, the device is logged out of the network and the half-made account goes
   the failed-login way. The login page then says "This account is already in Kourier." (with
   `logged_out` pointing at that account's "Link Again"), offers no "Try Again" (it would only
   fail again) and a button back to the list ("OK", or "Go to Chats" for a logged-out
   account). No unique index enforces it (`own_ids` is a JSON column, and a profile made before
   the rule may hold two `Active` accounts of one identity, which keep running: L4's "Remove
   Account" is how the user gets rid of one).
7. **Remove account**: `Logout` (best effort, 10 s; the Signal backend deletes its device on the
   server and exits `LoggedOut`; when it has an account and no socket, because it has just
   linked, is offline or is held, it opens a socket of its own for the request, so a removal
   while offline takes up to about 9 s instead of returning at once), `Shutdown`, delete the
   account secret from the
   Secret Service (this is the shred, §5.2), delete the `accounts` row (cascades core rows),
   `PRAGMA wal_checkpoint(TRUNCATE)` on `core.db`, then delete the directory.
   The server is always asked (link feedback L4): with no actor running (restart backoff,
   `Halted`) one is started (`StartMode::Resume`, no `Connect`) only for the `Logout`; a
   `Removing` row found at start (a quit cut the removal off) gets one such logout attempt
   before its shred (`Plan::Remove`), unless the first run kept a report that says the device
   is gone: what the logout told is kept as JSON in `accounts.state_reason` of the `Removing`
   (and `Unlinking`) row, written BEFORE the shred (link feedback L5), because a kill after the
   shred leaves no keys for another logout. The start that finds a kept `Removed` or `NoDevice`
   tells it without a new logout; a kept `NotReached`, a kept "nothing to tell" or no report
   gets ONE attempt (the keys may still be here: a shred that failed, a kill before it; the
   network may answer now) and the better of the two outcomes is told (Removed, then NoDevice,
   then NotReached); with the keys gone the attempt cannot start and the kept report stands.
   The start writes the kept report back with the state in one statement (`set_state` clears
   it). An account whose keys are gone and that kept nothing gets no notice.
   The `Logout` reports what became of the device
   (`BackendEvent::Logout(LogoutReport { outcome: Removed | NoDevice | NotReached,
   device_name })`, ephemeral, sent just before the `LoggedOut` exit; Signal stores the name it
   linked with in `backend.db`, migration 0022, NULL for older links). The Signal backend counts
   a 2xx answer to `DELETE /v1/devices/<id>` and a 401, 403 or 404 to it on an open socket (the
   server no longer knows the device or its credentials), and a 401 or 403 (or "device
   deregistered") at the door of a socket opened for the request, as `Removed`; a 5xx, 429,
   413, any other refusal (a 404 at the door too), a timeout or a transport error is
   `NotReached`. The core tells the front-end once, AFTER the removal is done (after the shred
   and the deletion of the row, BEFORE the wait for the derive permits and the removal of the
   account directory, which a big account takes a while for; the window in which a kill loses
   the notice is then the return of one store write):
   `UiEvent::LogoutOutcome { seq, account, removed, report, who }`; no report = `NotReached`, a
   logged-out account's removal = `NoDevice`. A shred or a row deletion that fails keeps the
   `Removing` row and tells nothing; the next start finishes the removal and tells then, so the
   user never reads "removed from this computer" about files that are still there, and never
   twice. `who` (`AccountLabel`) is the account as the UI names it, read from `core.db` before
   the row goes: name, number, device, the day the device was linked (`accounts.created_at`,
   which moves to the time a link is accepted) and `twin` (another account of the network has
   its identity or looks the same). An outcome told before the front-end said `UiReady` (a
   removal or unlink that the start finished runs before any front-end listens) is kept
   (`Shared::logout_outcomes`, at most 32) and told again at `UiReady` with the same `seq`; a
   front-end shows one notice per `seq`. The UI shows Removed/NoDevice as a passing notice
   ("The account “%1” was removed from this computer.") and NotReached as a dialog that stays,
   naming the device to remove on the phone. The notices are shown one dialog at a time (the
   next opens when the open one closes), none while the window is hidden in the tray (shown
   when it is back), and the shell (`Main.qml`) keeps them while no window exists (released,
   or a `--background` start) and hands them, with the open dialog's notice, to the next
   window; a window released with a dialog open hands that notice over too. KNOWN LIMIT: the
   notices held in memory (hidden window, no window) are lost when Kourier quits before a window
   shows them; the core replays only what was told before `UiReady`, so a NotReached instruction
   that never reached a visible window is not told again.
   The account id goes into `Shared::removing` before the row says `Removing`: attachment
   derivation (`attachments/derive.rs`) checks it before each compute and each commit, and the
   removal takes every derive permit around the directory delete, so a compute already
   running cannot write a thumbnail after it. Of two `Active` accounts of one identity (step
   6) only the removed one's device is logged out and only its rows and files go. The UI tells
   such twins apart by what the phone shows of each: the device number and the day it was
   linked ("device 3 · linked 12 Sep") in Settings → "Accounts", the drawer's tooltip and the
   questions, and the notice for a twin whose server could not be reached names the device's
   name and the day ("remove “desk” linked on 12 Sep from the linked devices on your phone"),
   never "this computer", which would be both entries.
   **Unlink** (`UiCommand::Unlink`, "Unlink…" for an `Active`, not logged-out account whose
   link has finished) is durable like a removal: the account is marked `Unlinking` in `core.db`
   before anything, the `Logout` goes to the running actor (or to one started for it), then the
   account goes the `LoggedOut` way (chats kept and readable, "Link Again"), with the same
   once-told outcome. A start that finds `Unlinking` (the logout was cut off by a kill) makes
   one logout attempt (`Plan::Unlink`) and logs the account out the same way; the outcome is
   kept on the row before the credentials are wiped, as a removal's is before its shred, so a
   kill between the wipe and the new state still tells what the first logout told.
8. **Rebuild `core.db`** (lost, corrupt, or a failed migration the user chose to reset): create an
   empty `core.db`; for each `accounts/<uuid>/key.wrap`, unwrap it (network kind and uuid are in its
   header), recreate the `accounts` row as `Active` and spawn with `start = Rebuild`. The backend
   re-emits its mirrored state into `core_outbox` (contacts, groups, settings, Storage Service
   records; Signal also re-requests contact sync). Messages lived only in `core.db` and come back
   only where the network re-delivers history (WhatsApp); identities and sessions are intact.

### 4.4 Login flows

```rust
pub struct LoginStep { pub step_id: String, pub txn: u64, pub kind: LoginStepKind }
pub enum LoginStepKind {
    ShowQr { data: String, valid_for: Option<Duration> },       // may repeat (rotation)
    ShowCode { code: String },                                  // WhatsApp pair code
    ConfirmEmoji { emoji: Vec<String> },                        // Threema rendezvous path hash
    Input { fields: Vec<InputField> },                          // device name, password, phone
    Progress { label: String, fraction: Option<f32> },
    Done,
    Failed { error: BackendError },
}
```

The UI has one generic `LinkAccountPage` that renders any step (§8.3); a relink opens it from the
account's banner. Signal: `Input{device name}` (prefilled with the hostname) → `ShowQr` repeated
every 45 s (max 6) → `Progress("Linking")` → `Progress("Syncing contacts and groups")` → `Done`.

### 4.5 Connection state

```rust
pub enum ConnState {
    Offline { retry_at_ms: Option<u64> },
    Connecting,
    Syncing { fraction: Option<f32> },        // catch-up, until the boundary below is committed in core.db
    Online { detail: Option<String> },        // e.g. Threema "leader"
    NeedsUserAction(UserAction),              // Relink | UpdateApp | SolveChallenge{url} | FixClock{skew_ms} | Reconnect
    Restricted { until_ms: Option<u64>, reason: String },   // WhatsApp temporary ban, reach-out lock
    LoggedOut { reason: String },
}
```

**Catch-up boundary** (contract rule). When the server says its backlog is delivered (Signal
`QueueEmpty`, Threema `ReflectionQueueDry`, WhatsApp `OfflineSyncCompleted`), the backend records
its current stage-1 arrival counter as the boundary. It stays in `Syncing` until every row up to
the boundary has passed stage 2 *and* been acked by the core, then switches to `Online`. Messages
emitted while `Syncing` (arrival ≤ boundary, or boundary not yet known) carry `catch_up = true`.
Signal Desktop does the same: `#onEmpty` waits until
the incoming, decrypted and app queues are all drained
(`SD/ts/textsecure/MessageReceiver.preload.ts:771-835`).

Core rules: the outbox drains as soon as the connection is up (`Syncing` or `Online`, also during
a history transfer's delivery; as Signal Desktop, whose conversation job queues wait only for the
socket, `SD/ts/jobs/helpers/commonShouldJobContinue.preload.ts` `waitForOnline`; link feedback
L6, the user's decision 2026-10-12; until then it drained only in `Online`). `catch_up` messages
never notify one by one, and one summary ("12 new messages in 4 chats") is shown when the account
leaves `Syncing` (Signal Desktop pauses notifications while catching up,
`SD/ts/background.preload.ts:2135-2154`). Backend side-effect queues (delivery receipts, retry
requests, profile-key responses) also wait for `Online` (R:signal-desktop-receive §9 item 27).

*The exception for a challenge.* A pause that waits for the user (a 428 challenge, rejected
credentials) is lifted by what lifted it before sends could start in `Syncing`: the challenge's
own end (its Retry-After, an answer, the user's retry) or the `Online` of a NEW connection after
the one the 428 came on dropped. The `Online` that ends the catch-up the 428 was met in is the
same connection, not a return after an interruption: it neither lifts the pause nor hides the
challenge, which also stays on show over the catch-up's own progress updates
(`kourier-core/src/dispatch.rs`, `paused_in_catch_up`).

*A known limit.* A send made while a history import still fills its chat sorts behind every
imported row that is already there and behind those that come later, because an imported row
sorts by its `sent_at` (§3.9) and the send is newer than all of them. An imported row dated
LATER than the send (a phone whose clock runs ahead, by less than a day: the mapper clamps only
what is further ahead) sorts after the send; that stays so.

### 4.6 Error model

```rust
pub struct BackendError { pub kind: ErrorKind, pub detail: String /* log-safe */, pub user_message: Option<String> }
pub enum ErrorKind {
    Transient { retry_after: Option<Duration> },   // network, 5xx, timeouts
    RateLimited { retry_after: Option<Duration> }, // 413/429
    Challenge { token: String, url: String,        // Signal 428 → needs the user (captcha)
                retry_after: Option<Duration> },   // the server's Retry-After: the pause ends by itself then
    Unauthorized, LoggedOut,                       // account-level
    PeerUnregistered,                              // recipient gone
    UntrustedIdentity { user: UserId },            // safety number changed; needs ConfirmIdentity
    TooLarge, InvalidInput, Unsupported, Internal,
}
```

Libraries use `thiserror`; `anyhow` only in the bin. `detail` follows the log-safety rules (§9.1).

### 4.7 Cancellation, shutdown, reconnect

- `CancellationToken` tree: app → account → operation. `Command::Shutdown` → backend stops
  accepting work, finishes the current durable step (never mid-transaction), emits nothing more,
  exits; the core waits 5 s, then cancels the token, then joins the thread. A `Shutdown` during
  a login is the end of that login, because the core discards an unfinished link at its next
  start without a logout: the Signal backend stops the login and removes a device the server
  had already linked for it (3 s of the 5, §6.5). A core that shuts down keeps every hold on
  the disappearing sweeper to the end (§9.6).
- A dropped `oneshot` reply means only "the caller stopped waiting"; it never aborts a send that
  is already on the wire.
- Reconnect is backend-owned (only the backend knows which errors are transient). The core
  forwards hints: `NetworkChanged` (NetworkManager `StateChanged` or a change of its
  `PrimaryConnection`, over D-Bus) and `Resumed` (logind
  `PrepareForSleep(false)`), the failure Flare hit (stuck offline after suspend, Flare #297). The
  hints of a burst are collected for 1 s (`kourier_core::HINT_SETTLE`) and reach each linked
  backend as one command (`Resumed` when a resume was among them); `Resumed` also wakes the
  core's own timers, which did not run while the machine slept. The core takes the hints as
  `UiCommand::NetworkChanged` and `UiCommand::Resumed`. They are sent by
  `kourier_desktop::hints::SystemHints`: three match rules on the system bus, no polling, the
  sender checked by the bus; a missing logind or NetworkManager costs nothing and logs nothing
  (§13 phase H). A bus that goes away is connected to again (first try after 500 ms, doubling
  to 60 s, only while a bus that was there is gone) and its return counts as one
  `NetworkChanged`. A machine with no system bus at start is noted once and waited for in the
  same way, with waits that double up to 10 minutes (`hints::ABSENT_MAX`): where a bus never
  comes, that is one connect attempt every 10 minutes, which fails at once.

### 4.8 Outbox and retry semantics (core)

1. **Enqueue** (one `core.db` transaction for up to 64 messages): insert the message row
   (`out_state = Queued`, `origin = OwnThisDevice`, `net_author = ''`), allocate
   `(local, sent_at)` with `Network::allocate_message_id`, insert the `outbox` row with the
   `OutgoingRequest`. Attachments are imported into the blob store first (encrypted copy, metadata via `kourier-media`). One
   user action can become several messages, stored all or none (also across a crash), each
   with its own strictly increasing id and its own outbox row, sent in order through the
   conversation's FIFO: `SendFiles` is split by `ConversationFeatures::plan` (§3.8), with the
   caption and the quote on the first message, or first as a text of its own where that message
   takes no caption; a text or caption above `max_text_bytes` is cut into several texts
   (`Text::split`: between lines or words, formatting kept). An edit cannot be split and is
   refused above that size. Up to `ENQUEUE_CHUNK` (64) messages are one transaction. A larger
   send is stored in several, so the one writer thread is never held for long: its rows are
   `Staged` (the scheduler does not see them) until the last transaction turns exactly this
   send's rows into `Queued`. The send owns its rows by message id, not by conversation, so a
   merge of two chats of one person between two parts is followed; a send that fails part way
   is dropped by its message ids (`outbox::discard_staged`), one that was killed by state at
   the next start (`discard_all_staged`). A chat that was hidden from the list (deleted and
   empty, `conversations.hidden`) stays hidden until the transaction that queues the send, so
   a dropped send leaves it as it was. While it is being stored, a timeline query can show
   its first messages as queued; `FilesStored` and the change events come only at the end.
   Compose commands run in one FIFO per conversation (`runtime::Lanes`): `SendText`,
   `SendFiles`, `SaveDraft`, `SetTyping`, Retry, Cancel, Edit, Delete, React,
   `DeleteAttachment`, `ClearConversation` and `DeleteConversation` of one chat keep their
   order and wait behind a running import of that chat; nothing waits for another chat, and
   there is no order across chats. `SetConversationTimer`, `ChangeConversation`,
   `DownloadAttachment` and `SubmitChallenge` are not in the lanes.
2. **Schedule**: one FIFO per conversation; the head runs only when the account is `Online`.
   `Command::Transmit` is one attempt; the backend does its internal repair inside it (Signal
   409/410 device mismatch, unsealed fallback). A look at the outbox reads one head per
   conversation along the partial index `outbox_waiting`, not every waiting row (a send of
   thousands of files is thousands of rows of one conversation). Rows behind a head that
   something else removed (ingest, the sweeper) start at once: whoever removes it kicks the
   account's dispatcher.
3. **Outcome** (written to `send_state` per recipient in the same transaction as the state change):
   - `Sent` → state Sent; next item.
   - `Partial{delivered, failed}` → delivered members `Sent`; if any failure is retryable, the item
     stays at the head with `only = <retryable failed members>` and `is_recipient_update = true`
     (unchanged when nobody was delivered: the retry is still the first send), under the backoff
     below (a `RateLimited` member pauses the conversation queue); a member that failed with
     `Challenge` pauses the account as in the `Challenge` case below; a member that failed with
     `UntrustedIdentity` pauses the conversation queue as in that case below, with the row
     narrowed to the waiting members (a challenge among the failures comes first); members that
     failed for good make the message `PartiallyFailed`.
   - `Transient` / `RateLimited` → the conversation queue pauses; backoff 0 ms, 190 ms, ×1.9, capped
     at 15 min, total budget 24 h (Signal's `conversationJobQueue`,
     `SD/ts/jobs/conversationJobQueue.preload.ts:339-340`); then `Failed{retryable}`.
   - `Challenge` → the *account* outbox pauses, `ConnState::NeedsUserAction(SolveChallenge)`, until
     the backend accepts the user's answer (`UiCommand::SubmitChallenge` →
     `Command::SubmitChallenge` with the token of the paused attempt), the backend is `Online` anew,
     the user retries, or the actor restarts within the same run. When the error carries
     `retry_after`, the pause also ends by itself at that time: the waiting sends are tried once and
     pause again if the challenge still stands. The challenge survives a restart of Kourier: `{url,
     token, retry_at, since}` is a `settings` row (scope `account:<id>`, key `challenge`), written
     in the transaction that records the refused attempt (or by the dispatcher for a challenge a job
     announced). The first actor of the next run takes it up for a linked account when it is not
     older than 24 h (Desktop's `DEFAULT_EXPIRE_AFTER`, `SD/ts/challenge.dom.ts`): the account shows
     it again, nothing is sent into it, the answer carries the stored token and the retry time
     counts from the server's answer. It is forgotten at a login or relink, at a later actor start
     of the same run and with the account. An answer the backend accepted forgets it by a write
     of its own directly behind the backend's reply, before the pause is lifted (the reply is no
     durable event, so no transaction can hold both; a kill between the reply and that write
     shows the solved challenge once more). The wait counts against each row's 24 h budget, but only
     the part between the challenge's arrival and the earlier of its own end (`retry_after`) and the
     moment the core looks (`outbox::Wait { conv, since, until }`): a row whose budget ended in that
     interval gives up (a message becomes `Failed{Challenge, retryable}`, a modifier is rolled back
     and reported); a budget that ended before the challenge came, or after the challenge's own end
     (the machine slept through it, or Kourier was closed), is still owed the one late attempt every
     overdue row gets. That attempt is the row's last: answered with a challenge or a safety number
     it fails at once (retryable) while the account still shows the challenge. After more than a day
     offline, the rows that were in flight when the challenge arrived (at most 4, one per
     conversation) therefore need a manual Retry; rows not yet attempted wait and go out after it.
     Waiting offline or logged out is not bounded. A challenge a backend job met
     (`BackendNotice::ChallengeRequired`) shows the same account state, holds no send, is stored the
     same way, and stays on show through a user's Retry.
   - `UntrustedIdentity` → the conversation queue pauses until `ConfirmIdentity` or the user
     cancels. The wait is bounded like a challenge's: rows of the paused conversation whose
     budget ends during it give up. This pause lives in memory: after a core restart the head
     is tried once and pauses again.
   - Anything else → `Failed{retryable: false}` for that message; the queue continues (a permanent
     failure does not block later messages, unlike matrix-rust-sdk's wedge, because Signal
     receivers order by arrival anyway).
   - The answer is recorded for the attempt's own row (`outbox.id` is never reused) and read by
     what the attempt sent. Message and conversation are read from the row, so a conversation
     merge during the transmit is followed. Row gone (an echo finished it): a send's per-recipient
     outcome still goes to `send_state`. Message gone: nothing is recorded.
4. **Restart**: rows in `Transmitting` go back to `Queued` and are resent with the *same*
   `local`/`sent_at`, `only` = recipients not `Sent` in `send_state`; the backend also skips
   recipients its own `send_progress` records as accepted (§6.7), so a crash between the server's
   acceptance and the core commit causes no second delivery. Receivers dedup by the id anyway
   (Signal: author + timestamp). Desktop resumes half-finished sends the same way
   (`sendStateByConversationId`, `SD/ts/jobs/helpers/sendNormalMessage.preload.ts:160-505`).
5. **User actions**: "Retry" on a failed message re-queues it with the same id and `only` = the
   failed members; deleting a queued message that nobody has removes the message and its outbox
   row if it was never attempted (a delete for me also when it was); a delete for everyone of a
   message that waits for a retry leaves a tombstone and sends the delete, because the earlier
   attempt may have reached the server; a message whose transmit is in flight cannot be deleted
   until the answer is there. A message nobody has (queued and never attempted, or refused by
   the network for good) is edited in place while it has no revision yet; such an edit is local
   and not bound by the chat's edit window, count or `features.edit`; with an edit already
   waiting, further edits are revisions behind it. A send that gave up on its budget after
   attempts without a definite answer counts as "may be with the server", like one waiting for a
   retry: a delete for everyone of it is sent and an edit is a held revision, and "Retry" keeps
   one attempt on record so this holds until the retried send is answered. Edits and reactions
   of a message whose send failed for everyone are kept locally and their rows are held: "Retry"
   sends the message first, then the held rows in order, with a fresh budget.
6. **Echo dedup**: an incoming own message (`OwnOtherDevice`) with the key of an outbox row merges
   into that row.
7. **Edits, deletes, reactions**: an own edit, delete for everyone or reaction is validated against
   the conversation's features, applied locally at once and queued as an outbox row of its target
   message (`kind` Edit | Delete | React) with its own network id; it shares the conversation's
   FIFO, backoff, budget and pauses. A change the network refuses for good is rolled back from
   the row's `undo` snapshot and reported with `ActionFailed` (a delete for everyone stays a
   tombstone for this account only); a change that already reached some recipients stays. A
   delete for me is local; its sync to own devices is best effort. Queued rows of a tombstoned
   message other than deletes are deleted with the tombstone, not sent (one in flight goes with
   its answer); a queued delete always stays. An edit names its target when it is sent (§3.5.3).
   Own-message disappearing timers start when the first recipient has the message, not at enqueue.

**Durable commands.** Read receipts, viewed receipts and read syncs must survive a crash (else the
phone keeps the unread notification; Desktop persists them in `readSyncJobQueue` and
`conversationJobQueue`, R:signal-desktop-receive §1). The core transaction that marks messages
read also inserts a `pending_commands` row; the core sends `SendReceipts`/`SyncReadMarker` with
that `cmd` id; the backend inserts the job into its `jobs` table (unique on `cmd`) and, in the same
transaction, `CommandPersisted{cmd}` into `core_outbox`; ingesting it deletes the pending row. At
startup the core resends every remaining row. A logout deletes the account's pending rows (what
was not confirmed by then is never sent), and the core queues no read receipt for a blocked chat,
a blocked sender in a group or an unaccepted request. Marking a chat read skips the messages
that are hidden on an unconfirmed clock (§9.6): the read sync and the receipts name only what
shows, and a hidden message is still unread when it shows again. Marking a chat read also
clears the "marked unread" flag another device set (`marked_unread`; Signal Desktop does the
same when a chat is read), also when that flag was all there was to read; such a chat has no
message to tell anybody about, so no durable command is stored for it. The network is told
about the cleared flag with `ChangeConversation { SetMarkedUnread { false } }`, not durable
and with no answer waited for: a backend that cannot write roaming state (Signal's, until
Storage Service writes exist) refuses, the chat stays read on this device and the phone
keeps its mark until it reads the chat itself. Deletes for me made in Kourier
(`ClearConversation`, `DeleteAttachment`) are durable commands of the same kind, stored in the
transaction that deletes locally. None is stored for a logged-out account, where the features
have no `delete_for_me`, for a clear that removed nothing a person sent and does not remove the
chat, or for an attachment without `net_ids`. Delivery receipts are backend-internal jobs (Signal
batches them 500 ms / ≤ 100, `SD/ts/util/deliveryReceipt.preload.ts`); typing stays
fire-and-forget, and the core sends none for a blocked chat, an unaccepted request, a chat with
`cannot_send`, Note to Self or a chat whose features have no typing.

### 4.9 Idempotent ingest (core)

Each account's forwarder delivers `Envelope`s in `core_outbox` order (up to ~100 outbox rows per
Envelope); the core applies each Envelope in one `core.db` transaction, then completes the `Ack`.
The writer thread schedules live envelopes and UI commands before history chunks and round-robins
across accounts, so a WhatsApp history import never delays Signal live ingest. Rules per event:

| Event | Apply |
|---|---|
| `Message` | Resolve aliases (author through `identity_alias`); find or create the conversation; upsert by the message key (§3.1). A key recorded in `expired_messages` within the last 45 days, by the disappearing sweeper or by a clear of the chat, is dropped (a late copy must not bring an expired or cleared message back). Existing row: if it is a placeholder → replace body, keep `sort_key`; if it is an own outbox row → merge state; else ignore (duplicate, including the same Signal (author, timestamp) in another conversation). New row: assign `sort_key` from `received_at`/`arrival` (§3.9), update unread/mentions, link `linked_local`, then apply matching `pending_modifiers` in arrival order. Decide notification (§8.7; `catch_up` → summary) |
| `HistoryBatch` | Same upsert, `history = true`, never notified and never in the catch-up summary (a live message that later fills an imported undecryptable placeholder is a live message and is announced), `sort_key` from `sent_at`; applies `initial_out_state`, `read`, `reactions` and `expiry` (as for a live message) and `edits` (history only). THE ROWS ARE THE TRUTH about unread: a row that says `read: false` is known to be unread and counts for `unread` and `mentions` like a live row (reading the chat here, a read marker of another device, a delete and a timer lower the count by the row; a row delivered twice counts once). `ConversationPatch.unread_hint` adds only unread messages the backend has no rows for; a backend whose rows are of unknown read state says `read: true` and uses the hint. `edits` (a NEW row, or one that fills a placeholder): one revision key per version and the "edited" mark, so a later reaction, receipt, quote or edit that names the newest version's id finds the row. Timers without a start: an own row that was not sent (queued, sending, failed) keeps its timer unstarted until its echo says it was sent (the Signal mapper gives a SENT own item without a start the import's time; a start that is given stands); a notice of a remote origin starts an after-read timer at ingest; a call line takes the `read` and `expiry` its signal states |
| `Edited`/`Deleted`/`Reaction`/`Receipts`/`ReadMarker`/`OwnSendState` | Resolve the target through current and revision keys and `alternates`: a modifier that states a conversation resolves only to a message of that conversation, also under `AccountAuthor` scope, where ids match account-wide (else a member of one chat could edit, delete or react in another); `conv = None` (receipts, read syncs) stays account-wide; `TargetAuthor::Unknown` matches on `local` within the conversation. Found → validate and apply: states only move forward; a receipt from another user counts only for an own message whose direct peer, group member or send-state holder that user is (else it is parked 2 days without effect and tried again whenever its sender can have become a recipient: at `IdentityMerged`, which keeps a receipt from a contact's ACI for a message in the PNI-keyed chat, at a `ConversationUpsert` that adds a member, at `OwnSendState` and at the answer of the message's transmit; a third party's receipt on someone else's message is dropped); reactions replace per user when `reactions_per_user = 1`, and a reaction longer than 64 bytes is dropped; edits only from the author, of a body that can be edited (§3.5.3), within `edit_receive`, and an edit with `keeps_quote: Some(false)` removes the message's quote for good, in any arrival order; deletes only from the author within `delete_receive` or from an admin when `admin_delete` (`by: None` on another member's message = our own admin delete, §3.5.2); both windows are measured from the original's `server_at`, else `sent_at` (§3.8). Not found, or found in another conversation than the stated one → insert `pending_modifiers` row with expiry (receipts and read markers 2 days, like Desktop's `syncTasks`; others 7 days); it is never applied to the message elsewhere |
| `ConversationCleared` | Hard-delete every message of the conversation up to the newest of `up_to` that is here (timeline order) and remember each key in `expired_messages` (45 days). Kept: what arrived later, and an own message whose send row is in the outbox unless that row is `Failed` and some recipient has the message (a partly failed message goes with the clear and can be an anchor; one nobody has, or one queued for a retry, stays). A tombstone whose delete for everyone still waits becomes a hidden `ForMe` tombstone and its delete is still sent. With `remove` and no message a person sent left: the notices go too and the conversation is hidden (`conversations.hidden`) until a message is stored or queued there. Files are unlinked after the commit. A clear that finds none of its anchors is parked for 2 days with the conversation's newest timeline position at that moment: the first anchor to arrive deletes itself and everything up to that position, which is the in-order outcome. When an anchor was present, a late anchor deletes only itself. Idempotent. A clear made in Kourier follows the same rules |
| `AttachmentDeleted` | Remove the first attachment of the target whose `net_ids` contain one of the event's names, with its files; own attachments carry the names of `Network::attachment_net_ids`; a message left with neither attachment nor text becomes a `ForMe` tombstone; an unknown name changes nothing; for a message that is not here yet the event is parked 2 days and applied on arrival |
| `ContactUpsert`/`ConversationUpsert` | Field-wise patch; `members.full` replaces, else delta |
| `IdentityMerged` | Rewrite authors and members, add alias row; re-key the direct conversation, or `merge_conversation` when both exist (§3.3; the result is hidden only when both were); then try the parked modifiers of the merged chat again (`Apply::retry_pending`), so one sent under the contact's other identity lands |
| `CommandPersisted` | Delete the `pending_commands` row |
| `Typing` | Not persisted; forwarded to the UI with TTL; a `Message` from the same `(author, device)` clears it |
| everything | Emits `UiEvent` invalidations after commit |

Property tests replay every scripted stream twice and in permuted order and require identical
final DB state; the conformance rules are in §10.2.

### 4.10 Backfill and history

- History is local-first: the UI pages `core.db` with a keyset cursor `(sort_key, id)`.
- The source is per conversation (`ConversationFeatures.history`, default from the descriptor).
- `HistorySource::None` (Threema; Signal on a run with the offer switched off, §14.2 item 14):
  nothing else.
- `PushedOnLink` (WhatsApp; Signal by default, whose archive import, §14.2 item 14, is
  delivered the same way): `HistoryBatch` events during and after linking, with progress; the
  backend splits them into ≤ 500 messages per `core_outbox` row, so each is one bounded core
  transaction that the writer interleaves with live traffic.
- `OnDemand` (WhatsApp PDO, newsletters): when the user scrolls past the oldest local message, the
  core sends `FetchHistory` once per conversation and anchor.
- What is unread in an import is said by its rows (`IncomingMessage.read`), never by a count
  beside them; `unread_hint` is for unread messages without rows (a network that knows a
  chat's count and not its messages). When an imported chat is read here, our other devices
  are told about each imported message that was unread (`SyncReadMarker`, whose `up_to` is
  always a message); the SENDER of an imported message gets no read receipt, also with read
  receipts on. `core.db` schema 12 holds the partial index of unread rows, `read_state =
  'Unread'` alone: not `history`, and not `deleted IS NULL`, so the read marker of another
  device and a merge mark rows through it.

---

## 5. Storage

### 5.1 Files

```
$XDG_DATA_HOME/kourier/            (~/.local/share/kourier, 0700; KOURIER_HOME overrides all dirs)
  core.db                          SQLCipher, core schema, all accounts
  accounts/<uuid>/key.wrap         header (version, network, uuid) + AEAD-wrapped backend.db key (§5.2)
  accounts/<uuid>/backend.db       SQLCipher, backend-owned
  accounts/<uuid>/blobs/<2 hex>/<random id>   kourier-blob files (attachments, thumbnails, avatars)
$XDG_CONFIG_HOME/kourier/kourier.toml        non-secret app prefs (log level, proxy, UI prefs)
$XDG_STATE_HOME/kourier/logs/kourier.log     rotating, redacted logs; crash reports
$XDG_CACHE_HOME/kourier/                     (~/.cache/kourier, 0700) Qt's QML disk cache (qmlcache/) and
  outgoing/<random>/<folder>.zip             a folder packed for the composer (plaintext, 0600, on disk; §5.8)
$XDG_RUNTIME_DIR/kourier/                    single-instance lock, media/ plaintext cache (tmpfs)
```

`--profile <name>` switches to `kourier-<name>` directories; `KOURIER_HOME=<dir>` puts everything
under one directory (tests, `dev-data/` which `.gitignore` already lists; the cache is its
`cache/`). `Paths::create_dirs` makes the cache directory 0700 like the others; only `outgoing/`
is wiped at start and at exit.

### 5.2 Keys and the Secret Service

- Master key: 32 random bytes, stored with oo7 in the Secret Service under attributes
  `{application: "app.kourier.Kourier", kind: "master-key", profile: <name>}`.
- [V] On this machine `org.freedesktop.secrets` is served by gnome-keyring-daemon, not KWallet's
  `ksecretd` (R:qt-kirigami §7.3). Kourier works with either; which one the user wants is an open
  decision (switching is a system change the user makes).
- `core.db` key = HKDF-SHA256(master, info = `"kourier core.db v1"`).
- Per account: a random `db_key` (backend.db) is stored only in `accounts/<uuid>/key.wrap` as
  AES-256-GCM(KEK, random nonce, db_key), header as associated data. KEK = HKDF-SHA256(ikm = master,
  salt = account secret, info = `"kourier account kek v1" ‖ uuid`). The account secret is 32 random
  bytes in its own Secret Service item `{application, kind: "account-secret", account: <uuid>,
  profile}`. Reasons: `core.db` can be lost or rebuilt without losing any account's identity
  (§4.3 step 8), and removing one item shreds one account.
- Blob keys are random per file and stored in `core.db` (`files.key`), living and dying with their
  rows; `secure_delete = ON` and `wal_checkpoint(TRUNCATE)` after removals and bulk deletes keep
  deleted keys out of free pages and WAL frames.
- What shredding guarantees: after "Remove account", that account's `backend.db` (identity keys,
  sessions, group secrets) cannot be decrypted from any surviving disk block, even with the master
  key, because its KEK needs the deleted account secret; the limit is old blocks of the keyring
  file itself (encrypted with the login password). Deleted messages are gone from the live files,
  but SSD blocks written before the deletion stay decryptable with the master key.
- No Secret Service, or it is locked and the user declines: passphrase mode. The master key is
  derived with Argon2id from a passphrase entered at startup (salt + params in `kourier.toml`); each
  account secret is then kept in its `key.wrap` header, so shredding an account relies on deleting
  that file. There is no plaintext fallback.
- A master key is made only for a profile without data (`Paths::has_data`): the Secret Service
  path fails with `MasterKeyMissing`, the development keyring of debug builds
  (`KOURIER_DEV_KEYRING=passphrase`) with `DevKeyringMissing`. Under a new key the account
  directories would not open, and a start that cannot open them must not decide their fate: an
  account directory whose `key.wrap` does not open with the present key is left untouched
  (`VaultError::WrongKey`), and only one whose secret is gone is removed as shredded.
- In memory: keys are `Zeroizing<[u8; 32]>`; SQLCipher gets them as a raw key
  (`PRAGMA key = "x'<64 hex>'"`, like Desktop, `SD/ts/sql/Server.node.ts:891-893`), so no PBKDF2 at
  open.

### 5.3 SQLCipher engine (decision)

- [V] libsignal-net links BoringSSL statically (`boring-sys`, `signalapp/boring` tag
  `signal-v5.2.0`), and that fork has no symbol-prefix feature (its `[features]` are `fips`, `rpk`,
  `mlkem`, `allow-crl-extensions-bad-version`, `underscore-wildcards`, `relax-cert-validation`).
  SQLCipher's OpenSSL provider uses OpenSSL APIs with the same names, so the two clash in one
  process (presage `README.md:35` says the same).
- Decision: SQLCipher 4 built with a **RustCrypto crypto provider**
  (`libsqlite3-sys` feature `bundled-sqlcipher-custom-crypto`), from `boxdot/rusqlite` rev
  `a2f393913d…`, the rev gurk pins together with the "disable atexit cleanup" fix
  (`refs/gurk-rs/Cargo.toml:133-137`) [V]. rusqlite 0.38.0 matches libsqlite3-sys 0.36 [V].
- Fallback with the same file format: SQLite3 Multiple Ciphers in SQLCipher-v4 mode (one C file,
  own crypto, no OpenSSL). It needs our own vendored `libsqlite3-sys`; no maintained crate does this
  today (wire core-crypto's attempt, PR #2582, was closed unmerged) [V web]. `kourier-db` hides the
  engine, so switching is a dependency change, not a data migration.
- Pragmas on open: `journal_mode = WAL`, `synchronous = FULL` (a commit must survive power loss
  before we ack a server or complete an `Ack`), `foreign_keys = ON`, `busy_timeout = 5000`,
  `cache_size = -8000`, `temp_store = MEMORY`, `secure_delete = ON`. Batching (§5.6) keeps the
  fsync count per message well below one.
- Qt is the third TLS stack: QtQml links QtNetwork, whose TLS plugin loads the system
  `libssl.so.3` on first use; BoringSSL's OpenSSL-named symbols in our dynamic symbol table
  (`-rdynamic`, a linker choice) could get bound by it [I: not observed]. S1 checks: the fork's
  `sqlcipher-crypto-provider` compiles against our RustCrypto versions, and one binary runs a
  libsignal-net TLS handshake, a SQLCipher query, `QSslSocket::supportsSsl()` and a QtNetwork
  HTTPS request in one process; `cargo xtask symbols-check` (§2.4) guards the symbol table.

### 5.4 `core.db` schema (initial migration, abridged)

```sql
CREATE TABLE schema_version(component TEXT PRIMARY KEY, version INTEGER NOT NULL);
CREATE TABLE accounts(
  id INTEGER PRIMARY KEY, uuid TEXT NOT NULL UNIQUE, network TEXT NOT NULL,
  state TEXT NOT NULL, own_ids TEXT NOT NULL /* JSON */, own_device INTEGER, display_name TEXT,
  settings TEXT NOT NULL DEFAULT '{}', local_prefs TEXT NOT NULL DEFAULT '{}',
  caps TEXT, sort_order INTEGER, created_at INTEGER NOT NULL);   -- no keys here: key.wrap (§5.2)
CREATE TABLE contacts(
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  user_id TEXT NOT NULL, profile_name TEXT, system_name TEXT, nickname TEXT, avatar_file INTEGER,
  trust TEXT, fingerprint BLOB, blocked INTEGER NOT NULL DEFAULT 0, hidden INTEGER NOT NULL DEFAULT 0,
  unregistered_at INTEGER, identifiers TEXT /* JSON */, meta BLOB, updated_at INTEGER,
  UNIQUE(account_id, user_id));
CREATE TABLE identity_alias(account_id INTEGER NOT NULL, alias TEXT NOT NULL, user_id TEXT NOT NULL,
  PRIMARY KEY(account_id, alias));
CREATE TABLE conversations(
  id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts ON DELETE CASCADE,
  conv_key TEXT NOT NULL, kind TEXT NOT NULL, is_self INTEGER NOT NULL DEFAULT 0,
  title TEXT, avatar_file INTEGER, muted_until INTEGER, archived INTEGER NOT NULL DEFAULT 0,
  pinned_order INTEGER, blocked INTEGER NOT NULL DEFAULT 0, request_pending INTEGER NOT NULL DEFAULT 0,
  marked_unread INTEGER NOT NULL DEFAULT 0, parent_id INTEGER, timer_secs INTEGER, features TEXT, features_hash INTEGER,
  local_muted_until INTEGER, draft TEXT, unread INTEGER NOT NULL DEFAULT 0,
  mentions INTEGER NOT NULL DEFAULT 0, read_up_to INTEGER, last_activity INTEGER, meta BLOB,
  UNIQUE(account_id, conv_key));
CREATE INDEX conversations_list ON conversations(account_id, archived, pinned_order, last_activity DESC);
CREATE TABLE members(conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  user_id TEXT NOT NULL, role TEXT NOT NULL, PRIMARY KEY(conversation_id, user_id));
CREATE TABLE messages(
  id INTEGER PRIMARY KEY AUTOINCREMENT /* since migration 0003: = LocalMsgId, never reused */,
  account_id INTEGER NOT NULL, conversation_id INTEGER NOT NULL
    REFERENCES conversations ON DELETE CASCADE,
  net_scope INTEGER NOT NULL /* 0 AccountAuthor, 1 Conversation */,
  net_author TEXT NOT NULL /* '' = own */, net_local TEXT NOT NULL,
  origin TEXT NOT NULL, device INTEGER, linked_message_id INTEGER,
  sort_key INTEGER NOT NULL, sent_at INTEGER NOT NULL, server_at INTEGER, received_at INTEGER NOT NULL, arrival INTEGER,
  body_kind TEXT NOT NULL, body TEXT NOT NULL /* JSON Body */, preview TEXT,
  reply_to TEXT /* JSON ReplyTo */, revision INTEGER NOT NULL DEFAULT 0, edited_at INTEGER,
  deleted TEXT, out_state TEXT, read_state TEXT, expiry_timer INTEGER, expires_at INTEGER,
  view_once INTEGER NOT NULL DEFAULT 0, history INTEGER NOT NULL DEFAULT 0, mentions_me INTEGER NOT NULL DEFAULT 0,
  meta BLOB, raw BLOB);
-- the message key (§3.1): account-wide for Signal, per conversation for WhatsApp and Threema
CREATE UNIQUE INDEX messages_key_account ON messages(account_id, net_author, net_local) WHERE net_scope = 0;
CREATE UNIQUE INDEX messages_key_conv ON messages(conversation_id, net_local) WHERE net_scope = 1;
CREATE INDEX messages_timeline ON messages(conversation_id, sort_key DESC);
CREATE INDEX messages_expiring ON messages(expires_at) WHERE expires_at IS NOT NULL;
CREATE TABLE message_revisions(message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  revision INTEGER NOT NULL, account_id INTEGER NOT NULL, net_author TEXT, net_local TEXT,
  body TEXT NOT NULL, edited_at INTEGER, PRIMARY KEY(message_id, revision));
CREATE INDEX revisions_by_key ON message_revisions(account_id, net_local, net_author);
CREATE TABLE reactions(message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL, emoji TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY(message_id, user_id, emoji));
CREATE TABLE receipts(message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL, kind TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY(message_id, user_id, kind));
CREATE TABLE send_state(message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  user_id TEXT NOT NULL, state TEXT NOT NULL /* Pending|Sent|Failed */, error TEXT, at INTEGER NOT NULL,
  PRIMARY KEY(message_id, user_id));
CREATE TABLE files(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, path TEXT NOT NULL,
  key BLOB NOT NULL, size INTEGER, mime TEXT, purpose TEXT NOT NULL /* attachment|thumb|avatar */);
CREATE TABLE attachments(id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  idx INTEGER NOT NULL, kind TEXT NOT NULL, mime TEXT, size INTEGER, file_name TEXT, width INTEGER,
  height INTEGER, duration_ms INTEGER, blurhash TEXT, waveform BLOB, caption TEXT, state TEXT NOT NULL,
  remote BLOB, file_id INTEGER REFERENCES files, thumb_file_id INTEGER REFERENCES files,
  UNIQUE(message_id, idx));
CREATE TABLE pending_modifiers(id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, conv_key TEXT,
  target_author TEXT /* NULL unknown, '' own */, target_local TEXT NOT NULL, kind TEXT NOT NULL,
  payload TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX pending_by_target ON pending_modifiers(account_id, target_local);
CREATE TABLE outbox(id INTEGER PRIMARY KEY AUTOINCREMENT /* since migration 0004: the dispatcher keys
    attempts in flight by it */, message_id INTEGER NOT NULL REFERENCES messages ON DELETE CASCADE,
  conversation_id INTEGER NOT NULL REFERENCES conversations ON DELETE CASCADE,
  kind TEXT NOT NULL /* Message | Edit | Delete | React */, request TEXT NOT NULL /* JSON OutgoingRequest */,
  state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at INTEGER,
  deadline_at INTEGER NOT NULL, last_error TEXT,
  undo TEXT /* since 0003: JSON rollback of an edit, delete or reaction */);
CREATE INDEX outbox_by_message ON outbox(message_id);
CREATE INDEX outbox_by_conversation ON outbox(conversation_id, id);   -- since 0003; the FIFO is min(id)
CREATE TABLE pending_commands(id INTEGER PRIMARY KEY /* = CmdId */, account_id INTEGER NOT NULL
  REFERENCES accounts ON DELETE CASCADE, command TEXT NOT NULL /* JSON */, created_at INTEGER NOT NULL);
CREATE TABLE settings(scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(scope, key));
```

The block shows the initial migration except where a comment names a later one; other columns and
tables that later migrations added are listed per phase in §13 (core schema version 9 after phase J:
0005 adds `conversations.timer_version` and `attachments.long_text`, 0006 the table
`expired_messages`, 0007 `conversations.hidden` and `accounts.number`, 0008 rebuilds `accounts`
with an AUTOINCREMENT id, so an account id is never handed out again, and adds the partial index
`outbox_waiting(conversation_id, id) WHERE state IN ('Queued', 'Transmitting')` and the table
`download_orphans`, 0009 adds `messages.expiry_hidden` and the partial index
`messages_expiry_hidden(account_id) WHERE expiry_hidden = 1`, §9.6). Outbox rows have a fourth
state `Staged` (a large send whose messages are still being written, §4.8 step 1), stored in
the same `state` column. The `settings` table also holds the one option the core keeps for the
user: scope `core`, key `expiry_policy` (§9.6).

`core.db` keeps no planner statistics (nothing runs `ANALYZE`). A read that must use a
particular index says so in its SQL (the sweeper's `+account_id`, §13 phase J), and a unit
test pins the plans of the sweeper's reads.

Not in M1, reserved: `messages_fts` (FTS5) for search, `persons`.

Access: one writer thread; all writes go through a `CoreStore` job channel with two priorities
(live envelopes and UI commands, then history chunks; round-robin across accounts inside each), and
are batched. Three read-only connections: two for UI queries, one for media-handle misses (§8.6).
The Qt thread never touches the DB (§8.5).

### 5.5 `backend.db`: backend-private state

- Opened only by its backend through `kourier-db` with `AccountContext.db_key`; its migrations ship
  in the backend crate (`crates/kourier-signal/migrations/`).
- Every backend has `core_outbox(seq INTEGER PRIMARY KEY, envelope BLOB NOT NULL /* versioned
  serde Vec<BackendEvent> */, created_at INTEGER NOT NULL)` from `kourier-db` (§5.6).
- The Signal backend owns (§6.3 for access rules):

| Table | Content |
|---|---|
| `account` | ACI, PNI, E164, device id, password, registration ids (ACI, PNI), profile key, master key / AEP, storage key, manifest version, `post_link_sync` state, arrival counter, catch-up boundary, `device_name` (the name the device was linked with, what the phone lists; NULL for a link made before schema 22, migration 0022) |
| `own_identity` | our ACI and PNI identity key pairs (never generated by us; from provisioning) |
| `identities` | remote identity keys + verified state + first use |
| `sessions` | `(our_service_id, their_service_id, device_id) → SessionRecord` |
| `prekeys`, `signed_prekeys`, `kyber_prekeys`, `kyber_triples` | ours, per identity; triples guard Kyber last-resort reuse (`LS/rust/protocol/src/storage/traits.rs:129-139`); `confirmed` on signed and last-resort keys = the server has the key (rotation goes by the newest confirmed one, §6.12) |
| `sender_keys` | `(address, distribution_id) → SenderKeyRecord` |
| `sender_key_info`, `sender_key_devices` | our sender-key chain per group: distribution id and start time; the member devices that hold it (device id, registration id) |
| `inbox` | durable decrypted envelopes (§5.6): `arrival`, `received_at`, lane key, content attempts, `parked_until`, metadata |
| `msl_payloads`, `msl_recipients`, `msl_message_ids` | Message Send Log, 14-day retention; `msl_payloads.group_id` names the group a payload was sent in (a resend adds our distribution message) |
| `send_progress` | per outgoing `(local, recipient)`: accepted by the server; a repeated `Transmit` skips these (§6.7) |
| `attachment_uploads` | pointers of attachment uploads, keyed by SHA-256 of what was uploaded and how, reused for 3 days and purged hourly (§6.7 step 2). Each pointer holds the attachment key, also after its message was deleted or expired |
| `peers` | per contact: profile key, access key, sealed-sender state, expiring profile-key credential, last profile fetch, unregistered_at, `profile_sharing` (whitelisted), `share_my_number`, the 1:1 chat's disappearing timer and its version (`expire_timer`, `expire_timer_version`) |
| `sent_quotes` | the encoded quote of every reply our account sent (from this device, and from other own devices out of their sync transcripts), keyed by the reply's network id and by each edit's id: an edit must carry the quote again (§6.7). Personal data; purged after 14 days with the Message Send Log |
| `groups` | master key, secret params, revision, decrypted state snapshot, endorsements + expiry, last fetch, `dirty` |
| `group_credentials` | 7-day auth credential window |
| `storage_records` | raw decrypted Storage Service records (kept for the M2 write path) |
| `jobs` | backend job queue (sync requests, read receipts and syncs with the core's `cmd` id (unique), delivery receipts, profile refresh, group refresh, key maintenance, resend requests, deletes for me made here); `challenged` and `held_from` mark the jobs a send challenge holds (§6.12) |
| `identity_links` | which ACI a PNI belongs to, as far as this device learned it (§6.13); one funnel writes it and emits `IdentityMerged` once per pair |
| `number_links` | which ACI a phone number belongs to, as the Storage Service told this device (§6.13, schema 21); `links::learn_number` emits `IdentityMerged{number → aci}` only when the pair is new or the number moved |
| `remote_config`, `sender_certs` | `/v2/config` (ETag) and the two sender certificates |
| `retry_state` | decryption-error dedup and session-reset rate limits |

### 5.6 Durability protocol (stage 1, stage 2, forwarder)

```
server ──envelope──▶ STAGE 1   one backend.db transaction per batch:
                       decrypt (ratchet, prekeys, sender keys, Kyber triples, identity save)
                       INSERT inbox(arrival, received_at, lane, plaintext)
                       INSERT core_outbox(facts derived here: identity replaced, PNI→ACI verified)
                     COMMIT ──▶ ack to the server
                     STAGE 2   per lane, in arrival order:
                       network steps first, time-boxed, outside any transaction (group logs, profile)
                       one backend.db transaction: apply side effects (group state, peers, storage
                         records, jobs) + DELETE the inbox rows + INSERT core_outbox(their events)
                     FORWARDER one per account, in seq order:
                       core_outbox rows ──▶ Envelope{events, Ack} ──▶ core: one core.db transaction,
                       idempotent ──▶ COMMIT ──▶ Ack(Ok) ──▶ DELETE core_outbox rows ≤ seq
jobs (profile refresh, Storage Service sync, group refresh, key maintenance): fetch first, then one
  transaction that applies the result and INSERTs core_outbox (same rule as stage 2)
restart: inbox rows → stage 2 again (none of their side effects committed, since they are deleted
  in the same transaction); core_outbox rows → forwarded again (absorbed by idempotent ingest)
```

Invariants (all backends; WhatsApp and Threema adapters in §7):
1. A backend transaction that changes state writes the core events it implies in the same
   transaction (§4.1 principle 6). Events computed by diffing against backend state (group system
   messages, identity changes, storage merges) are therefore never lost by a crash.
2. An inbox row is deleted in the transaction that writes its events into `core_outbox`.
3. No network I/O inside a transaction; network results are inputs to it.
4. `core_outbox` rows are deleted only after the core's `Ack(Ok)`.
5. No cross-file transaction anywhere (SQLite gives no atomicity across WAL files,
   R:multi-protocol-prior-art §4.4).

Throughput: stage 1 commits once per batch (≤ 30 envelopes), stage 2 once per lane step (≤ 50
rows when no network step intervenes), the forwarder ships ≤ 100 outbox rows or 50 ms per Envelope
(one core transaction, one Ack, one `DELETE … WHERE seq <= ?`). That is well under one fsync per
message, which the catch-up budget (§1.3) needs.

- Send side: ratchet state and the MSL entry are committed in `backend.db` *before* ciphertext
  leaves; the core outbox row holds the stable id; acceptance per recipient is recorded in
  `send_progress` (§6.7).
- Poison rows: only *content* failures (decode, validation, handler errors) count; after 10 of them,
  or when the row is 45 days old, it is dropped and a `Placeholder` is emitted (Desktop's limits,
  `SD/ts/sql/Server.node.ts:6317, 6426-6460`). Network failures never count: they park the lane
  (§6.6).
- Catch-up boundary: §4.5.

### 5.7 Migrations

- Each component (core, each backend) has `migrations/NNNN_name.sql`, embedded with
  `include_str!`, forward-only, run in one transaction at open, recorded in `schema_version`.
- Rust-side data migrations (rare) are numbered steps in the same list.
- Every migration is tested against a fixture DB from the previous release (`tests/fixtures/`), and
  a downgrade (newer DB than binary) refuses to open with a clear message.
- JSON columns use `#[serde(default)]`; enums are stored by name; `BackendBlob.v` versions backend
  payloads.

### 5.8 Attachments and the blob store

- Format `kourier-blob v1`: header (magic, version, chunk size 64 KiB, 7-byte random nonce prefix),
  then AES-256-GCM STREAM chunks (`aead::stream`, last-chunk flag), key random per file. Detects
  truncation and tampering; allows streaming reads.
- Writes are atomic (`<id>.tmp` then rename, fsync). Files are 0600. Two kinds of file skip the
  per-file fsync: thumbnails (received and outgoing) are never fsynced, because they can be
  made again from their blob (one pass per start checks each by its length,
  `kourier_blob::sealed_len`, and derives a broken one again); files imported for a send are
  fsynced in batches of 1024 and at the end, always before the transaction that enqueues the
  send.
- The thumbnail (long edge 400 px, JPEG q80 or PNG with alpha, derived after a download and at
  the import of a picture) is a smaller copy for the sizes it covers, not the picture the user
  sees: the timeline draws the file where the thumbnail would be stretched (§8.6, "Which file a
  timeline picture is drawn from").
- Incoming: core decides auto-download (images, stickers, voice notes and files ≤ 25 MiB by
  default; videos and bigger files on click; never above the network's receive limit), issues
  `DownloadAttachment` with a fresh `BlobTarget`; the backend verifies the network's MAC/digest and
  streams plaintext into the blob writer. An attachment flagged `long_text` is the rest of a
  message's text: the core downloads it whatever the settings say (≤ 256 KiB; not from blocked
  or request-pending chats, not history), puts the text into the message and removes the
  attachment; the long texts of a chat are fetched when it stops being blocked or
  request-pending. A download whose attachment row goes while it is in flight (message
  deleted, expired or cleared, one attachment deleted) is remembered in `download_orphans`, and
  the file is removed when the backend's answer comes.
- Outgoing: the core imports the user's files into the blob store, computes metadata (dims,
  blurhash, duration) with `kourier-media`; the backend reads the blob, applies its own transcoding
  (Signal: still pictures re-encoded without metadata at Signal's level three, 4096 px and at
  most 3 MiB, `SD/ts/util/scaleImageToLevel.preload.ts`; not Desktop's default level one of
  1600 px), encrypts per protocol and uploads. The import takes any number of files: every file
  is inspected before any is copied, and if one is refused (unreadable, not a file, above the
  size limit, a kind the chat does not take) nothing is queued, no blob stays and the refusal
  names each such file by position. Files are copied one after the other in 64 KiB chunks; only
  one picture at a time is held in memory (up to 64 MiB, for thumbnail and blurhash; a larger
  picture goes without them). An empty file is sent like any other. A file marked "as original
  file" is copied undecoded as a generic `File` with its true MIME type and name and gets no
  thumbnail; the backend sends its bytes untouched. While it copies, the core reports
  `Progress::Import` for the send's request id (first at 0, last at all files, at most about
  ten a second between).
- Plaintext appears only in `$XDG_RUNTIME_DIR/kourier/media/` (tmpfs, 0700) for AnimatedImage,
  video and audio playback, when the user saves/opens a file, and for images pasted into the
  composer (`paste/`, 0600, until removed from the strip, or after sending for at least 120 s
  and for as long as the core still prepares files for that conversation). Media files are
  deleted when the view closes,
  at exit, and at startup. Still pictures and avatars are decrypted in memory only (§8.6).
  A hidden message (§9.6) has no playback copy: hiding removes an open one.
- One plaintext file is written to the disk, not to the tmpfs: the archive of a folder the
  user sends as one .zip (`kourier_app::folders::ArchiveFiles`, §8.3). It lies in
  `$XDG_CACHE_HOME/kourier/outgoing/<24 random hex digits>/<folder>.zip` (directory 0700, file
  0600). Reasons: the runtime directory is memory, an archive can be as large as the chat's
  file limit (100 MiB on Signal) and several can wait; a folder's files already lie on a disk
  in the clear, unlike a pasted picture; and backups leave the cache directory out. The
  archive is removed when its row leaves the composer, once the core has copied it for the
  send, at exit and at the next start (a crash leaves it until then). The store has two
  locks: `gate` is held while a place is made on the disk and during the wipe, by worker
  threads and the exit only, and its value says the store is closed; `dirs` guards the list
  and is never held across disk I/O, because the UI thread takes it (`owns`, `take`).
- Retention: files live as long as their message; deleted/expired messages delete their files in
  the same transaction (unlink after commit). Avatars are replaced in place. A "storage used" view
  and per-conversation cleanup come after M1.

---

## 6. Signal backend (`kourier-signal`)

### 6.1 Foundation decision

**Decision: write Kourier's own Signal service layer on libsignal v0.104.0, using
`libsignal-protocol`, `zkgroup`, `libsignal-account-keys`, `signal-crypto` and the networking crates
`libsignal-net`, `libsignal-net-chat`, `libsignal-net-grpc`. Port behavior from Signal Desktop
(the spec). Do not depend on presage or libsignal-service-rs (lss); use lss only as reference
code, and copy small AGPL pieces from it with attribution where they fit.**

Why, against the alternatives (R:rust-signal-ecosystem recommended lss first; R:libsignal and
R:signal-desktop-send recommended libsignal-net):

| Criterion | presage | lss fork | own layer on libsignal-net (chosen) |
|---|---|---|---|
| Ack after commit | no (lss acks before decrypt, `messagepipe.rs:94-105`) | needs patch (#442 open) | built in: `ServerEvent::IncomingMessage.send_ack` is a `Send` closure the app calls when it wants (`LS/rust/net/src/chat/server_requests.rs:15-29`) [V] |
| Hermetic tests on `@signalapp/mock-server` | no | no: lss is HTTP/1.1 only (`http1_only`, reqwest-websocket); the mock serves websockets only over HTTP/2 extended CONNECT (`SD/packages/mock-server/src/api/server.ts:287-303`) | yes: Desktop drives the same mock with libsignal-net over HTTP/2 (R:signal-desktop-receive §7.4-7.6) |
| libsignal version | lss's pin (v0.99.0) | lss's pin; one tag per graph | our pin, v0.104.0, bumped with Desktop |
| Group send | per-member fan-out | fan-out (sender keys only as draft PRs #453/#472) | `send_multi_recipient_message` with `MultiRecipientSendAuthorization::Group(GroupSendFullToken)` (`LS/rust/net/chat/src/api/messages.rs:140-200`) [V] |
| gRPC migration | none | none | handled by libsignal-net with remote-config overrides; missing key = gRPC, fronted routes fall back to WS (`LS/rust/net/chat/src/ws.rs:173-185`) [V] |
| Groups v2 | legacy `/v1/groups` | legacy `/v1/groups`, no logs, no endorsements | `/v2/groups`, logs, endorsements, ported from Desktop |
| Fit with unified store | poor (own `ContentsStore`) | good | best |
| Code we write for M1 | least, then rework | moderate + fork upkeep | most: ≈15-20k lines Rust [I] |

What makes the larger code size acceptable:
- Most M1 logic must be written by us in *every* option: the durable receive pipeline, MSL and
  retry, Storage Service merge rules, group state handling, the outbox, sender keys with
  endorsements (R:rust-signal-ecosystem §6.2 lists these as lss gaps).
- Desktop is a complete, current, AGPL reference; every module below names the Desktop file it ports.
- The parts of lss that are pure logic (provisioning cipher, device-name crypto, profile cipher,
  attachment cipher, storage-service item crypto, groups-v2 decryption helpers) are short and can be
  ported to libsignal 0.104 APIs with attribution (both AGPL-3.0-only).

Costs and how they are contained:
- `libsignal-net*` is explicitly not a stable API (`LS/doc/src/net/README.md`); the bridge layer
  holds the connection manager and some app policy. All libsignal-net use sits in one module
  (`kourier_signal::net`, ≈1.5k lines) behind Kourier types; the bridge code it copies is diffed on
  every bump (§6.15).
- Signal says use outside its apps is unsupported. Kourier sends an honest user agent
  (`Kourier/<ver> libsignal/<ver>`) and never imitates Signal-Desktop.

Fallback path if libsignal-net ever becomes unusable for a third-party client: `SignalTransport`
is a real trait from the walking skeleton (S2) on (`connect_auth`, `connect_unauth`,
`provisioning`, `request`, typed sends, `https`), implemented by `net::LibsignalTransport`; an
lss-style transport over plain WebSocket/HTTP can replace it without touching receive/send logic.
No third-party client is known to run libsignal-net chat directly (R:rust-signal-ecosystem §4.3), so
S2 exercises this choice against the real server early, with a dedicated test account.

### 6.2 Crate structure (modules → Desktop sources they port)

```
crates/kourier-signal/
  protos/            copies of SD/protos/{SignalService,SignalStorage,Groups,DeviceMessages}.proto
                     (header records Desktop commit); prost-build at build time (protoc 3.21.12 [V])
  migrations/        backend.db schema
  src/
    lib.rs           SignalNetwork: Network impl (descriptor, allocate_message_id, spawn)
    actor.rs         per-account thread, LocalSet, command loop, job scheduler,         (SD/ts/background.preload.ts)
                     stage-2 lanes, core_outbox forwarder
    zone.rs          synchronous SQL transaction + store overlay                         (SD/ts/SignalProtocolStore.preload.ts zones)
    store/           libsignal store traits over backend.db: identity, session, prekey,  (SD/ts/LibSignalStores.node.ts,
                     signed, kyber (+triples), sender key; ACI and PNI instances           SignalProtocolStore.preload.ts)
    net/             SignalTransport trait; ConnectionManager (env, DNS, routes,          (LS bridge net.rs, net/chat.rs:530-660;
                     remote-config overrides, network change), AuthChat, UnauthChat        SD/ts/textsecure/SocketManager,
                     (5-min rotation), raw REST, Https client (storage + CDN), keepalive   WebsocketResources.preload.ts)
    provisioning.rs  provisioning socket, QR URL, envelope decrypt, PUT /v1/devices/link   (SD/ts/textsecure/Provisioner.preload.ts,
                                                                                            ProvisioningCipher.node.ts, AccountManager)
    keys.rs          prekey generation, upload, counts, rotation, capabilities            (SD/ts/textsecure/AccountManager.preload.ts:617-739,
                                                                                            UpdateKeysListener.preload.ts)
    receive/         envelope decode, PNI rules, unseal, decrypt, inbox, content dispatch  (SD/ts/textsecure/MessageReceiver.preload.ts,
                                                                                            processDataMessage, processSyncMessage)
    send/            content builder, padding, 1:1 sender, sealed sender, 409/410,        (SD/ts/textsecure/SendMessage.preload.ts,
                     sync transcripts, group fan-out, sender key + GSE                     OutgoingMessage.preload.ts, util/sendToGroup.preload.ts)
    msl.rs, retry.rs Message Send Log, DecryptionErrorMessage both directions             (SD/ts/util/handleRetry.preload.ts,
                                                                                            handleMessageSend.preload.ts)
    groups/          credentials, presentations, state/logs fetch, decrypt, endorsements  (SD/ts/groups.preload.ts,
                                                                                            services/groupCredentialFetcher, util/groupSendEndorsements)
    storage.rs       Storage Service read + merge                                          (SD/ts/services/storage.preload.ts,
                                                                                            storageRecordOps.preload.ts)
    contacts.rs      contact sync blob                                                    (SD/ts/services/contactSync.preload.ts, ContactsParser)
    profiles.rs      versioned profile fetch, decrypt, avatars, sealed-sender state       (SD/ts/services/profiles.preload.ts)
    attachments.rs   CDN download/verify/decrypt, encrypt/pad/digest, upload form, TUS,    (SD/ts/AttachmentCrypto.node.ts,
                     GCS resumable                                                         util/uploads/tusProtocol.node.ts, WebAPI)
    config.rs        /v2/config with ETag, limits, gRPC overrides                         (SD/ts/RemoteConfig.dom.ts, LibsignalNetRemoteConfig)
    sender_cert.rs   two sender certificates, 1 h refresh buffer                          (SD/ts/services/senderCertificate.preload.ts)
    mapping/         proto ↔ kourier-model (ids, text ranges UTF-16↔UTF-8, bodies, events)
```

### 6.3 Threading and the protocol zone

- One OS thread per Signal account runs a `current_thread` runtime with a `LocalSet`. It owns the
  `backend.db` connection (`Rc<RefCell<rusqlite::Connection>>`) and the libsignal stores. The store
  traits are `#[async_trait(?Send)]` (`LS/rust/protocol/src/storage/traits.rs:48`) [V], so this is
  required.
- Network futures are `Send`; the actor runs them on `AccountContext.net_rt` with
  `net_rt.spawn(..)` and awaits the `JoinHandle` from the LocalSet.
- **Zone** = one `BEGIN IMMEDIATE … COMMIT` + an in-memory overlay of store writes applied to the
  store cache only on commit (Desktop's zones), behind a **synchronous** API:
  `fn zone<R>(&self, f: impl FnOnce(&mut Tx) -> Result<R>) -> Result<R>`. libsignal's async store
  calls inside it (`message_decrypt_*`, `message_encrypt`, `group_encrypt`, …) are driven with
  `now_or_never().expect("store future pended")`: our store impls are synchronous rusqlite, so their
  futures are always ready. Because a zone body cannot await, no other `spawn_local` task can run
  SQL on the shared connection while a transaction is open. Rules:
  1. Every operation that reads or advances protocol state (decrypt batch, encrypt for a send,
     session repair after 409/410, SKDM processing, identity save) runs inside a zone. All other
     `backend.db` access, reads by jobs included, goes through `zone` or `read` (a zone without
     writes); debug builds assert that zones never nest.
  2. No network I/O inside a zone (impossible by construction); a zone that rolls back has never
     let ciphertext out.
  3. Ciphertext produced in a zone leaves only after the zone committed.
  4. A zone that changes state writes the core events it implies into `core_outbox` (§5.6).
- Receive batches, stage-2 lanes, send tasks, jobs and the forwarder are `spawn_local` tasks that
  interleave between zones.

### 6.4 Network layer

- `ConnectionManager` (port of the bridge's, ≈300 lines; start from
  `LS/rust/net/src/chat.rs:491-582` `test_support::simple_chat_connection` and
  `LS/rust/bridge/shared/types/src/net/chat.rs:530-660`): `Env` (`env::PROD`, `env::STAGING`, or a
  local test env copied from `LS/rust/bridge/shared/testing/src/net_env.rs:81-137` with HTTP/2 and
  the mock CA as DER), `DnsResolver::new_with_static_fallback`, `ConnectState`, route provider
  (direct; proxy and domain fronting are later settings), user agent, network-change watch,
  gRPC override map.
- Chat connections: `ChatConnection::start_connect_with(...)` then `finish_connect(net_rt,
  pending, grpc_overrides, listener)` (`LS/rust/net/src/chat.rs:270, 382`) [V].
  - Authenticated: `ChatHeaders::Auth{aci, device_id, password, receive_stories: false}`. The
    listener is a synchronous `FnMut` (`LS/rust/net/src/chat/ws.rs:190`): it converts
    `ListenerEvent` → `ServerEvent` and forwards it into an **unbounded** channel to the actor. It
    cannot await, `blocking_send` panics on a runtime thread, and `try_send` on a full bounded
    channel would drop an envelope that is then never acked, stalling receive until reconnect. The
    server bounds unacked pushes [I]; backpressure is applied by delaying stage-1 batches and so
    their acks (§6.6).
  - Unauthenticated: created lazily, rotated 5 minutes after first use (Desktop
    `SocketManager.preload.ts:801-841`); used for sealed sends, prekey fetch with access key or
    endorsement token, unauth profile fetch, device link.
  - Provisioning: `CHAT_PROVISIONING_PATH`, no headers, `ProvisioningEvent` (`server_requests.rs:178-189`).
- Requests: typed `libsignal-net-chat` APIs where Desktop uses them (1:1 sealed/unsealed send,
  sync send, multi-recipient send, upload form, prekey fetch); raw REST over the chat socket
  (`ChatConnection::send`, `chat.rs:425`) for everything else, *including prekey upload/count and
  `PUT /v1/devices/capabilities`*, as Desktop does. Reasons: the mock server serves Keys/Devices only
  over REST, and fronted HTTP/1.1 routes have no gRPC (R:libsignal §6).
- Remote config: `GET /v2/config` with ETag at connect and every 2 h; `desktop.libsignalNet.grpc.*`
  keys become the override map (same as Desktop, `SD/ts/LibsignalNetRemoteConfig.preload.ts`); a
  missing key means gRPC (libsignal default). Scalar limits go to `AccountCaps`.
- `Https` (storage.signal.org for Storage Service and groups, cdn0/2/3): reqwest 0.13 with
  `rustls-no-provider`, a rustls config built with the *ring* provider and a root store that holds
  only Signal's CA from `SD/config/default.json:23` (Desktop passes it as `ca` for every
  `_ajax` call, `SD/ts/textsecure/WebAPI.preload.ts:429`) [V]. This keeps one rustls backend (ring)
  and no aws-lc-sys (R:libsignal §6).
- Keepalive and reconnect (Desktop `WebsocketResources.preload.ts:503-649`,
  `SocketManager.preload.ts:254-290`): `GET /v1/keepalive` 30 s after each answer with a 30 s
  timeout; on `Resumed` or `NetworkChanged` an open socket is asked at once with a 5 s timeout,
  and one silent for more than 5 min (wall clock) is replaced without asking; Fibonacci backoff
  1…55 s with up to 5 s jitter on delays longer than 5 s, extended to 1597 s while no attempt
  since the last reset got as far as an HTTP answer (Kourier has no "offline" signal); a hint
  cuts a wait short and resets the delays; `NetworkChanged` → `ConnectState::network_changed`.
  Close 4409 (connected elsewhere) → no auto-reconnect, `NeedsUserAction(Reconnect)`, lifted
  only by `Command::Connect`; close 4401 → reconnect, whose answer decides; 401/403 or
  `DeviceDeregistered` at connect → the unlink procedure of §4.3 step 5 (drain, wipe,
  `LoggedOut`); app-expired/499 → `NeedsUserAction(UpdateApp)`; clock skew above
  `client.maxAllowedClockSkewSeconds` (default 24 h, at least 1 h) →
  `NeedsUserAction(FixClock)` with the socket closed until the clock is right (§13 phase G).

### 6.5 Linking flow

1. `StartLogin{"qr", relink}` → `Input{device name}` (default: hostname) → user submits.
2. Open the provisioning socket with a fresh X25519 key pair; on `ReceivedAddress{address}` ack 200
   and emit `ShowQr{ "sgnl://linkdevice?uuid=<address>&pub_key=<pct-encoded std b64 of 33-byte key>&capabilities=" }`.
   Rotate every 45 s, keep ≤ 2 sockets, give up after 6 rotations (`SD/ts/textsecure/Provisioner.preload.ts:96-107`).
3. `ReceivedEnvelope` → verify version 1 and HMAC, then decrypt: ECDH → HKDF-SHA256(salt 32×0,
   info `"TextSecure Provisioning Message"`, 64 B) → AES-256-CBC + HMAC-SHA256
   (`SD/ts/textsecure/ProvisioningCipher.node.ts:52-155`); validate required fields (provisioning
   code, ACI identity, profile key, master key or AEP; PNI identity + number; reject E164-less
   accounts in M1 with a clear message). On a relink, an ACI other than `expect_own` fails here,
   before anything is registered (§4.3 step 6). A first link can be given the last digits of the
   number of the account it must find (`BackendConfig::expect_number_suffix`): another account
   fails here too, with nothing registered (§13 phase K).
4. Generate password (16 random bytes, base64), registration ids in `1..16383`, per identity a
   signed EC prekey and a signed Kyber last-resort prekey, encrypt the device name to the ACI
   identity key (`SD/ts/Crypto.node.ts:115-140`).
5. `PUT /v1/devices/link` on the unauth socket with Basic auth `<aci>:<password>`,
   `fetchesMessages: true`, capabilities `{attachmentBackfill: true, spqr: true,
   usernameChangeSyncMessage: true, optionalPhoneNumber: false}`: Desktop's values for an account
   with a number (`SD/ts/types/Capabilities.d.ts`, `SD/ts/textsecure/WebAPI.preload.ts:2962-2967`
   at link, `SD/ts/background.preload.ts:2091-2098` at every start). The server derives the
   account's capabilities from its devices, so Kourier claims no less than Desktop;
   `attachmentBackfill` is claimed because Kourier now does what it stands for: it sends
   backfill requests for files the server lost and applies the answers of the primary (§6.11
   "Asking the phone"). It never answers a request of another device (only the primary
   does), and it states the capability to its own core at every start (§13 phase K).
   409 → `Failed("Kourier is too old for this account")`, 411 → `Failed("too many linked devices")`.
6. Persist account, identities (own marked verified), keys, profile key, master key/AEP, storage key
   (`HMAC-SHA256(masterKey, "Storage Service Encryption")`) in one `backend.db` transaction,
   together with the server the link was made on (`account.env`: `production`, `staging` or
   `local:<host>:<port>`). Every later start is held to it: an account is refused, before
   anything connects and without any wipe, when the start names another server (§9.2).
7. Connect the auth socket; upload 100 EC + 100 Kyber one-time prekeys per identity (`PUT /v2/keys`);
   the mock server completes provisioning only after this (R:signal-desktop-receive §7.4).
8. Post-link sync: `/v2/config`; send `SyncMessage.Request{CONTACTS}`, `{CONFIGURATION}`,
   `{BLOCKED}` (and `{KEYS}` only if AEP/master key is missing) as unsealed 1:1 sends to our own
   devices (prekey fetch, session, `send_sync_message`); start the receive loop; run the Storage
   Service sync; emit `Progress` until contact sync and Storage Service sync are complete (Desktop
   waits for both, `SD/ts/background.preload.ts:1825-1982`), then `AccountReady` + `Done`.
   After an imported history archive the Storage Service sync does not hold `Done` (the archive
   named every chat; Desktop does the same): only the contact sync does
   (`connection::initial_sync_done`, `TransferShared::imported`, memory only, forgotten when a
   login starts: a later link waits for the sync unless its own transfer ends `done`). The
   gate (`finish_link`) is
   looked at by the job runner whenever no job is due AND after every batch (`jobs::run`), so
   profile fetches, group refreshes and receipts queued behind it never hold the login; the
   contact sync runs at priority 2, ahead of the group refreshes (3) an import queues (link
   feedback L2, 2026-10-12: on the mock at 150 ms round trip the gate waited ~19 s behind 50
   group refreshes and 300 profile fetches).
   `BackendConfig.initial_sync = LinkOnly` emits `Done` right after the sync requests are sent;
   tests use it until contact and Storage Service sync exist (S9), production uses
   `ContactsAndStorage`. The mock's `waitForSync` resolves when the phone has received all three
   requests (`SD/packages/mock-server/src/api/primary-device.ts:307-316, 2045-2050`).

A login is stopped, never aborted (`provisioning/link.rs`, `Actor::settle_login`).
`CancelLogin`, `Logout`, `Shutdown` and a `StartLogin` that follows a cancel ask it to stop
through a token, and it ends at the next point where it knows what the server holds. In steps 1
to 4 nothing is registered and it ends at once (`LinkEnd::Stopped`; no `Failed` step, no
warning). Once the request of step 5 is on its way, its answer is awaited, and a device the
server linked is handed back unstored (`LinkEnd::Unwanted`) and removed with
`DELETE /v1/devices/<id>` on a socket opened with its credentials. A link whose account cannot
be stored in step 6 takes the same path and then reports the failure. The waits are bounded:
`Logout` gives the login 4 s (`LOGOUT_LOGIN_SETTLE`) inside its 7 s of network time
(`LOGOUT_NETWORK`), a `StartLogin` after a cancel 4 s, `Shutdown` 3 s for the wait and the
removal together (`SHUTDOWN_LOGIN`); each is followed by the 2 s drain, which keeps `Logout`
inside the core's 10 s and `Shutdown` inside its 5 s. Past that the task is dropped with a
warning to check the phone: a device linked for a request whose answer never came has an id
nobody knows. `Logout` with an account and no socket (just linked, offline, held) stops the
connection task and opens its own authenticated socket for the `DELETE`; if that socket is
refused with 401, 403 or `DeviceDeregistered`, the phone has removed the device already and
there is nothing to do (an info line, no warning). `Logout` with neither a login nor an
account exits `LoggedOut` at once.

### 6.6 Receive loop

```
ServerEvent (listener on net_rt) ──unbounded mpsc──▶ actor (LocalSet)
STAGE 1: batch ≤ 30 envelopes or 75 ms (Desktop decryptAndCacheBatcher); waits while the inbox
         holds > 2,000 unprocessed rows (backpressure = delayed acks)
  zone {
    for each: decode Envelope proto (decode error → ack 500)
      PNI rules: on our PNI accept only PREKEY_MESSAGE; drop sealed sender to PNI; drop non-receipt from a PNI source
      SERVER_DELIVERY_RECEIPT → inbox row (no content)
      UNIDENTIFIED_SENDER → sealed_sender_decrypt_to_usmc → validate certificate at server timestamp,
                            reject own ACI+device → dispatch on USMC type (SenderKey → group_decrypt;
                            PreKey/Whisper → message_decrypt_*; Plaintext → DEM only)
      DOUBLE_RATCHET / PREKEY_MESSAGE → message_decrypt_signal / message_decrypt_prekey
      identity key replaced by save_identity → core_outbox(ContactUpsert{fingerprint, trust}
                            + Message(System IdentityKeyChanged)), sibling sessions archived
      unpad (0x80 then zeros); SKDM in Content → process_sender_key_distribution_message
      PniSignatureMessage → verify_alternate_identity → core_outbox(IdentityMerged)
      SyncMessage.pniChangeNumber → apply in-line (new PNI identity, keys, reg id)
      message addressed to our PNI → peers.share_my_number = true (§6.7)
      DuplicatedMessage → drop; other decrypt error → inbox row {decrypt_error, sender, device, ts, hint}
      INSERT inbox(arrival = ++counter, received_at = now, lane, plaintext Content, metadata)
  } COMMIT → send_ack(200) for each
QueueEmpty → catch-up boundary = counter (persisted); ConnState stays Syncing (§4.5)
STAGE 2: one lane per conversation (group id; peer ACI for 1:1 and for sync transcripts to that
         peer; "misc" for receipts, read syncs and other content without a conversation);
         order kept inside a lane, lanes run concurrently
  lane step: take ≤ 50 rows → network steps first, each time-boxed (10 s) → zone {
      Content dispatch (table §6.13): side effects (peers, groups, storage trigger, jobs)
      DELETE the rows; INSERT core_outbox(events; messages carry received_at, arrival, catch_up) }
  network failure: sender is a member under the last known group state → apply with that state and
      mark the group dirty (refresh job); otherwise park the lane (backoff 5 s … 10 min). Never a poison count.
  content failure: attempts++ (poison rule §5.6)
FORWARDER: core_outbox → Envelope{events, Ack} → core commit → Ack → DELETE
every row ≤ boundary acked by the core → ConnState::Online; resume side-effect jobs (delivery
  receipts, retry requests, profile-key responses)
```

- Session operations are serialized per `(our service id, sender, device)` by the zone; SKDMs are
  processed inside the decrypt zone, before later envelopes that need them.
- Lanes run handlers in per-conversation order, as Desktop's per-conversation `queueJob` does, and a
  failed group update holds back only that conversation
  (`SD/ts/messages/handleDataMessage.preload.ts:274-293`). Modifiers in the misc lane that
  overtake their target go to `pending_modifiers` in the core.
- Group messages: if `groupV2.revision` is newer, apply the embedded signed change (verify the
  notary signature, change epoch ≤ 7) or fetch logs (a stage-2 network step) before emitting the
  message; the group state update and its system messages commit with the message (§6.8). Drop
  messages from non-members, blocked senders, blocked groups, and non-admins in announcement-only
  groups.
- Validation per `R:signal-desktop-receive §9` items 18-25 (timestamps equal, flags, 32-byte
  master keys, typing rules); a message is not refused for its attachment count (Desktop and
  Android only cut the list at 32 for their own display); edit/delete windows (item 26) are
  checked by the core (§3.8).
- Blocked senders: everything a blocked sender's envelope held is dropped in one place after
  decryption (`dispatch_content`), undecryptable content included (no placeholder, no retry
  request), and no long text is downloaded for it; only the server's own delivery-receipt
  envelope is still applied, as in Desktop.
- Delivery receipts: jobs, sent only for sealed-sender messages in accepted conversations, never
  to a blocked contact (receipts owed to someone blocked since are forgotten), batched
  (500 ms / ≤ 100), after catch-up.
- Decryption failures (`retry.rs`): dedup per `(sender, device, timestamp)`, ≤ 5 attempts; send a
  `DecryptionErrorMessage` in `PlaintextContent` to the sender device; emit an `Undecryptable`
  placeholder with the would-be id `(sender, timestamp)` when the content hint is `Resendable`;
  fall back to archiving the session and sending a NullMessage (≤ 1 per hour per device).

### 6.7 Send path

`Transmit` handler (one `spawn_local` task per conversation, serial):

1. **Build `Content`**: `DataMessage{timestamp = sent_at, body, bodyRanges (UTF-16), attachments,
   quote, expireTimer + expireTimerVersion (1:1, always), profileKey (always; M1 rule below),
   groupV2 {masterKey, revision}, requiredProtocolVersion}`; reactions, deletes and edits use their
   fields; a body > 2 KiB goes out trimmed to 2 KiB with the whole text (≤ 64 KiB) as a
   `text/x-signal-plain` attachment in front of the others; pad to 80-byte blocks
   (`SD/ts/textsecure/OutgoingMessage.preload.ts:123-148`). Service ids sent binary, accepted in both
   forms. 1:1 sends, typing and receipts add `pniSignatureMessage`
   (`IdentityKeyPair::sign_alternate_identity`) while `peers.share_my_number` is set; it is set when
   that peer messages our PNI and cleared when the peer's delivery receipt arrives for a message
   that carried the signature (MSL `hasPniSignatureMessage`;
   `SD/ts/background.preload.ts:2660-2674`, `SD/ts/messageModifiers/MessageReceipts.preload.ts:289-305`).
   This is how contacts who know us only by number merge our ACI conversation.
   **Profile sharing (M1 rule):** Desktop sets `profileSharing` when the user sends the first
   message in a chat and writes it to the Storage Service (R:signal-desktop-send §5.5). Kourier
   cannot write in M1, so a 1:1 chat with `whitelisted = false` and no pending request gets
   `features.cannot_send = NotAccepted` ("Start this chat on your phone"); when the phone accepts,
   the Storage Service record flips and the backend re-emits the features. Sends therefore always
   carry our profile key.
2. **Attachments**: read blob → transcode (still pictures: level three, §5.8) → encrypt
   (AES-256-CBC + HMAC, `logPadSize` padding, digest; incremental MAC for mp4, kept on at most 10
   attachments of a message, remote key `global.maxIncrementalMacsPerEnvelope`, so the message
   stays under the server's 256 KiB; the GIF flag only on video content, never on an `image/gif`
   file) → upload form (`get_upload_form`) → CDN3 TUS or CDN2 resumable;
   cache pointers in `backend.db` (`attachment_uploads`, keyed by the uploaded source: the
   transform plus the core blob, not the plaintext hash Desktop uses) and reuse for 3 days
   (`SD/ts/AttachmentCrypto.node.ts:144-274`, `SD/ts/util/uploads/tusProtocol.node.ts:110-260`).
   Progress → `TransmitProgress` (share of blob bytes sent, below 1 until every upload is done).
   A `File` attachment, also one that holds a picture ("send as original file"), goes out
   untouched under its name and its true content type; only `Image` is re-encoded. The
   pointer's `clientUuid` is the name the core holds for the attachment
   (`Network::attachment_net_ids`), so an edit keeps it. A blob upload that moves no byte for
   60 s (`upload::STALL_TIMEOUT`) is dropped and the TUS or GCS resume takes over. Bytes are
   counted where the body is produced, into a queue of 8 chunks of 64 KiB; once every byte is
   handed over, the request gets 5 stall times (5 min) for the tail and the answer, because
   nothing counts the queued bytes (an upload smaller than the queue is in that state from the
   start). Sticker sends are refused in M1.
3. **1:1**: devices = sessions that pass `has_usable_sender_chain(now, NotStale |
   EstablishedWithPqxdh | Spqr)` (the policy flags live in libsignal's bridge and are copied,
   R:libsignal §3); any other device gets `get_pre_keys` + `process_prekey_bundle` (PQ prekey
   mandatory); zone{ `message_encrypt` per device; sealed sender via
   `UnidentifiedSenderMessageContent::new(.., ContentHint::Resendable, None)` +
   `sealed_sender_encrypt_from_usmc` when we have an access key or the peer is UNRESTRICTED; MSL row }
   → typed send (unauth sealed / auth unsealed) → zone{ `send_progress` row for the accepted
   recipient }. 409 → archive extra, fetch missing; 410 → archive and refetch stale; retry within
   the same Transmit (bounded); 401/403 on sealed → mark sealed sender DISABLED and resend unsealed;
   404 → `PeerUnregistered`; untrusted identity → `UntrustedIdentity`. A recipient already in
   `send_progress` for this `local` is skipped and reported as sent.
4. **Sync transcript**: `SyncMessage.Sent{timestamp, destinationServiceIdBinary (1:1),
   expirationStartTimestamp, message, unidentifiedStatus[], isRecipientUpdate}` to our own devices,
   unsealed, `urgent: false` (`SD/ts/textsecure/SendMessage.preload.ts:1497-1652`). Note to Self =
   sync only.
5. **Groups**: recipients = members minus us, blocked and unregistered contacts, restricted to
   `req.only` when set.
   - Primary path (sender key + Group Send Endorsements): `sender_key_info` (distribution id, reset
     after `desktop.senderKeyMaxAge` or a member removal); SKDM to new member devices (1:1, sealed,
     not online-only, with the content hint and urgency of the send it prepares: Resendable for a
     message, Implicit for typing); `group_encrypt`
     → USMC with the no-E164 sender certificate and group id → `sealed_sender_multi_recipient_encrypt`
     → `send_multi_recipient_message(payload, ts, Group(GroupSendFullToken), online=false,
     urgent=true)`; endorsements valid only between now+2 h and now+2 d, else refresh group
     (`SD/ts/util/groupSendEndorsements.preload.ts:136-155`); needs ≥ 2 eligible recipients. A
     member is eligible only with an endorsement and sessions whose identity is trusted for
     sending: `sealed_sender_multi_recipient_encrypt` does not check trust, so the backend does.
   - Fallback: per-member 1:1 sends (sealed with access key or the member's endorsement token, else
     unsealed, naming the group id in the sealed content) for ineligible members, or for every
     member when the multi-recipient send fails with 401 or with an answer the typed API does not
     know (the websocket route's 404). An untrusted member fails there with `UntrustedIdentity`.
     A 428 on the multi-recipient send is such an answer too: libsignal-net v0.104.0 hands no
     challenge over for unauthenticated requests, so its token is lost; the 1:1 sends that
     follow report their own 428 as `Challenge` with token and Retry-After.
   - Bounded concurrency (link feedback L6, `send/group.rs`): the key setup and the fan-out do
     not go member by member (100 members took 30 s on the mock at 125 ms round trip, 6 s now).
     Prekey fetches run up to `PREKEY_FETCHES_AT_ONCE` = 32 at once (Desktop 30), the
     distribution messages to new devices and the 1:1 fan-out up to `MEMBER_SENDS_AT_ONCE` = 16,
     all on the send's own task (no cross-task state); the per-recipient send locks stay, the
     409/410 repair loop stays one entry at a time (rare), a distribution is recorded per member
     as soon as the server took it. The sender certificate is fetched once before each of the
     parallel steps (one cache read when it is cached), never by every one of their sends. The
     send reports its step (`SendPhase` in `TransmitProgress`, §4.2, at most every 250 ms per
     step) for the line under the message (§8.4).
   - Results merged per recipient into `send_progress`; MSL covers both paths. Some recipients
     accepted and some failed → `TransmitOutcome::Partial`; the core retries only the failed ones
     with `is_recipient_update = true`, and the follow-up sync transcript sets `isRecipientUpdate`
     (Desktop excludes already-sent recipients the same way, R:signal-desktop-send §2.1).
6. **Receipts / typing / read sync** commands: `SendReceipts`/`SyncReadMarker` become `jobs` rows
   (with `CommandPersisted`, §4.8) that run after catch-up. `ReceiptMessage{READ|VIEWED}` only if
   the synced read-receipt setting is on. In a 1:1 chat: only when it is accepted and the
   contact is not blocked. In a group (Desktop `sendReceipts.preload.ts`): one 1:1
   `ReceiptMessage` per author, sealed with the author's access key or, when sealed sender is
   off for them, their group send token; the sealed content does not name the group, carries
   no PNI signature and is not urgent. Gates: the group is one we are a member of, not blocked
   and accepted; the author is not us, not blocked, not unregistered, and either accepted or
   without any 1:1 chat in the core. Blocked and left are checked again when the job runs. One
   job per command holds one batch per author; when a receipt has to wait (429, 428, network),
   the job's payload is rewritten to what is still owed, so a retry does not tell the authors
   already served again (a crash still can).
   `SyncMessage.read` to own devices always; `TypingMessage` (`Implicit`, `online: true`) only
   if the typing setting is on and never to a blocked contact, groups via sender key.
7. **Retry requests from others** (`retry.rs`): archive our session if the ratchet key matches; resend
   the MSL `Content` (+ SKDM for groups) if ≤ 14 days old, else send an SKDM or a NullMessage.

### 6.8 Groups v2 (read-only in M1)

- Master keys from Storage Service `GroupV2Record`s and from incoming `DataMessage.groupV2`; group
  id from `GroupSecretParams::derive_from_master_key`.
- Credentials: `GET /v1/certificate/auth/group?...` for a 7-day window, `AuthCredentialWithPniResponse::receive`;
  presentations as HTTP Basic auth to storage.signal.org.
- State: `GET /v2/groups/logs/{rev}` (with `joined_at_version` when unknown, pagination via 206),
  else `GET /v2/groups/`; decrypt title, description, timer, avatar, members, member profile keys
  (`SD/ts/groups.preload.ts:3585-3792, 6428, 7195, 7379`); 403 = not a member (on an invited row
  the invitation is over; a row that asked to join keeps its request), 404 = gone, 401 = the
  day's credential was refused (the credentials are forgotten and the job tries again).
- Endorsements from `GroupResponse` / last log page, stored with expiry.
- Mapping: `ConversationUpsert{title, avatar, members(full), timer, features{cannot_send: NotMember |
  InvitationPending | JoinRequestPending | AdminsOnly}}` + `System` messages for changes since the
  last known revision (not on first sync).
- Apply rule (§5.6): fetch logs/state first (network step), then one zone that stores the new
  revision and snapshot *and* inserts the `ConversationUpsert` and `System` messages computed from
  the diff into `core_outbox`. A crash can no longer leave the backend at the new revision with the
  core still showing old members and no system messages. A failed fetch marks the group `dirty`;
  a refresh job retries with backoff (5 s … 10 min) and, until then, messages from senders that are
  members under the last known state are applied with it (§6.6).

### 6.9 Contacts and Storage Service (read-only in M1)

- Contact sync blob (`SyncMessage.Contacts`): download the `AttachmentPointer` from the CDN with
  retries and decrypt it (receive-side attachment code, §6.11, built in S9), parse varint-delimited
  `ContactDetails` (name, ACI, number, avatar, initial expire timer, inbox position) →
  `ContactUpsert` (R:signal-desktop-send §5.2).
- Storage Service: credentials (`GET /v1/storage/auth`), manifest if newer than local version (204 =
  unchanged; **404 → do nothing**: a linked device never creates a manifest,
  `SD/ts/services/storage.preload.ts:1514` is a primary-only path), read missing items, decrypt
  (legacy `Item_` keys or `recordIkm` HKDF). Then, per ≤ 500 records, one zone merges
  `ContactRecord`, `GroupV2Record`, `AccountRecord` into `backend.db` *and* inserts the resulting
  events into `core_outbox`: `ContactUpsert` (profile key → `peers`; names, blocked, whitelisted →
  `profile_sharing` and `cannot_send`), `ConversationUpsert` (archived, muted, `marked_unread`,
  blocked), `PinnedConversations`, `AccountSettings` (read receipts, typing indicators, link
  previews, phone number sharing, universal timer), `IdentityMerged` for records that join ACI,
  PNI and E164. The manifest version is stored in the last of these zones, so a crash re-reads
  the remaining records. Raw records are kept for M2 writes. Triggered after link, at startup, and
  on `SyncMessage.FetchLatest{STORAGE_MANIFEST}`.
- M2 (not M1): write path with unknown-field preservation (prost drops unknown fields, so writes
  re-encode known fields and append the unknown field bytes carried from the raw record), 409
  handling, `FetchLatest` to other devices.

### 6.10 Profiles

Versioned unauth fetch with access key (`GET /v1/profile/{aci}/{version}[/{credentialRequest}]`
over the unauth socket), falling back to authenticated; decrypt name/about/emoji (AES-256-GCM with the
profile key); avatar from CDN0; update sealed-sender state from `unidentifiedAccess`
(`SD/ts/services/profiles.preload.ts:313-860`). Cadence: on new profile key, on first message from an
unknown contact, routine refresh ≤ 50 contacts not refreshed for 3 days, at most hourly. A fetch
can replace a contact's identity key (R:signal-desktop-send §4.1); the zone that stores the result
also inserts `ContactUpsert` (and `System IdentityKeyChanged` when the key changed) into
`core_outbox`. One avatar per contact is stored: the synced setting `preferContactAvatars`
decides whether the address-book photo or the profile picture wins when a contact has both, and
a change of the setting asks the phone for a contact sync or fetches the profiles again.

### 6.11 Attachments (receive)

`GET {cdn[N]}/attachments/{cdnKey}` (CDN0 path differs, as in Desktop) → stream through HMAC +
digest (or plaintext hash) verification → strip padding to `size` → `kourier_blob::Writer` (the
incremental MAC of videos is produced on send but not verified on receive: the whole-blob MAC and
digest are); limits from remote config (`global.attachments.maxReceiveBytes`, else 1.25 ×
`global.attachments.maxBytes` as Desktop); thumbnails and blurhash rendered core-side. Avatars
(profile, group) are CDN0 blobs decrypted with the profile key / group secret params.

- Results (`AttachmentDone`): network failures, 5xx and 429 are transient (retryable); a blob
  longer than its pointer allows, a pointer size over the receive limit and 413 are `TooLarge`; a
  wrong MAC, digest, padding or size and an unknown CDN number are `InvalidInput` (no request is
  made for an unknown CDN; Desktop falls back to CDN 0); 404 and 410, and a 403 of CDN 0
  (Desktop: "means the same as 404 from other CDNs"), are `Expired` ("No longer available").
  A pointer that does not pass `Pointer::from_proto` arrives `Unavailable` ("Not available on
  this device", no `remote`), not as a failed download whose "Retry Download" the core
  always refuses. When the
  transfer broke off, the transport error is reported whatever the decryptor made of the cut
  stream.
- A request that carries a blob (download or upload) has a whole-request timeout of max(10 min,
  length / 16 KiB/s); a streamed download that receives nothing for 60 s fails as a timeout.
  A download that breaks off goes on with `Range: bytes=N-` inside the same
  `DownloadAttachment` command (206 checked against `Content-Range`; of a 200 answer the first
  N bytes are skipped; a 416 on a resumed request means the blob ends there). One decryptor
  runs over the joined stream, so MAC and digest verify the blob as a whole. It gives up as a
  retryable failure after 5 requests in a row without a byte (pauses 1, 2, 4, 8 s) or 200
  requests; a first request that brings nothing fails as before. Nothing partial is kept on
  disk: the next command starts at byte 0. `DownloadProgress` reports the plaintext bytes
  written every 250 ms between the event at 0 and the final one.
- Long texts (`text/x-signal-plain`) are downloaded in the lane's network step within its 10 s
  time box (up to 3 tries when the transfer broke off) and mapped as the whole body; when that
  fails, the message keeps the trimmed body and the attachment stays with it flagged `long_text`:
  the core downloads it later and joins the text (§5.8).
- View-once media is not mapped: the message is `Unsupported` ("view-once"), no attachment is
  mapped and the raw DataMessage (pointer and key) is not kept, until the view-once flow exists.
- A pointer is checked by what it carries (`Pointer.integrity`): a wire pointer by its digest,
  an archive's (history import, `BackendBlob` v 3: a trimmed backup `FilePointer`) by its
  plaintext hash when it has one, else its digest; `Pointer::from_blob` reads v 2 and v 3 and
  refuses anything else before a request. The names of an attachment (`net_ids`) are
  `uuid:<hex>` (client UUID), `digest:<hex>` and `plaintext:<hex>` (an archive's plaintext
  hash): a delete-for-me sync is sent with `fallbackPlaintextHash` too and one that names
  only the plaintext hash is applied (most imported attachments have no digest). The names
  of a received delete are tried in the order the event lists them (client UUID, digest,
  plaintext hash, as Desktop's `deleteForMe`) over ALL the attachments of the message: two
  copies of one file share a plaintext hash, and the delete of the second must not take the
  first (`AttachmentDeleted` in `kourier-backend-api`, `attachment_deleted` in the core).
- Asking the phone (step S6, Signal's attachment backfill; user decisions of 2026-10-11: the
  button says "Download", the wait and its end are the attachment's own note, a long text is
  asked only with the files of its message). A click on an attachment that is "No longer
  available" (`Expired`), "Not available on this device" (`Unavailable`) or whose ask was not
  answered goes through the same `DownloadAttachment`; the core's `may_click` decides
  `Click::Download` or `Click::Ask` (size first; `can_ask`: account Active, the backend's
  `AccountCaps` flag `attachment_restore`, a message with a network id, not view-once or
  deleted, not a long text, and a chat and an author that are not known by phone number
  alone: the answer could not be placed, a number-only chat is read-only). The row's
  `MessageActions.ask_attachments` lists the attachments this decision is `Ask` for, one by
  one (a message with a file above the receive limit and a fine lost one lists the second
  only; one being asked for is not listed); the viewmodel copies the list to the blocks and
  decides nothing. The flag is the backend's own statement: the Signal backend sends its
  `AccountCaps` at every start (`maintenance::append_account_caps`), with the limits of the
  stored remote config when there is one, so it does not wait for a config. An ask stores
  one `attachment_requests` row PER MESSAGE (core.db schema 13; a second click joins it) and
  queues the durable `Command::RequestAttachments`; the attachment is `Asking` ("Asking your
  phone…"). The Signal backend sends an URGENT `SyncMessage.attachmentBackfillRequest` to our
  own devices (job kind `backfill_request`, the usual retry) naming the message by its author
  and ORIGINAL sent time (an edited message too: Android finds it by that time; the core
  then ignores the long-text slot of an edited message) and reports
  `BackendEvent::AttachmentRestore { Sent }`, or `NotSent` when it gave up or no session with
  device 1 exists. The job is dropped, with `NotSent`, when it is older than
  `backfill::REQUEST_MAX_AGE` (1 h) before it ran (no socket came up) as well as when a
  failed attempt would start after that (`jobs::expired_before_run`).
  "Asking your phone…" always ends. The request row has a deadline from the click
  (`UNSENT_BOUND_MS`, the backend's hour and five minutes), replaced at `Sent` by one 30 s
  later and by 2 min after each answer that still has a PENDING slot (`Timeouts::
  restore_answer` / `restore_pending`; a PENDING long text counts while this device asked
  and its text is still lost); the deadline is durable (`attachment_requests.deadline`,
  `kourier-core/src/restore.rs`), a wait whose deadline passed while Kourier was closed ends
  at the first look after its account's backend replayed its outbox (`restore::Gate`: 10 s
  after the actor started, or the first `Online`), so an answer that came in time and was
  waiting there is applied to its request first; a logout or unlink ends the account's open
  waits; a start ends an
  attachment that says it is being asked for with no request open; a due row whose message is
  gone is deleted. The gate also covers the start itself: `Core::start` marks the accounts
  that will get an actor as starting before the task is spawned, so its first look cannot
  end a due wait before the actor registered and replayed (the supervisor's own mark, at the
  spawn, replaces it). Ending a wait by any of these ways deletes the request's command from
  `pending_commands` too: a command no backend confirmed yet would otherwise go out at a later
  start, for a wait that is over. The backend's `Sent` and `NotSent` name that command
  (`AttachmentRestore.cmd`, kept in `attachment_requests.cmd`) and apply only to the request
  it made, never to a newer wait of the same message (a second click after the first wait
  ended); an answer names no command on any network and applies to the message's open
  request. All end as "Your phone did not answer…" (`AskFailed { NoAnswer }`,
  clickable again). Answers (`attachmentBackfillResponse`) are taken ONLY from device 1 of
  our account (Desktop and Android take any of our devices; a linked device cannot give a
  file a pointer here) and only for a message named with its conversation: the core finds
  the message only in that conversation. Matching in the core is two passes: every file
  slot by name (a slot carries `uuid:` and `digest:` names only, the client UUID and the
  digest of the file the phone re-uploaded; the plaintext hash is the STORED attachment's
  name, which checks the new pointer's download, never the slot's matching), then by
  position, only where the phone's order is proven to be Kourier's: the answer has exactly
  as many slots as the message has non-long-text attachments, every slot a name placed sits
  on its own index, and every file slot placed by position has the size of the file there
  (an unknown size 0 proves nothing; the import drops entries without a pointer). A "gone"
  slot states nothing about a file but its place, so it is believed only where a NAME placed
  a slot too: an answer of status slots alone ([gone, still uploading] with no name), or of
  file slots placed by their sizes alone, proves nothing about the phone's order and its
  "gone" slots place nothing. (A message of ONE file has no order to prove: its single
  "gone" slot is believed, or "not on your phone" would be out of reach for it.) A slot no pass placed is skipped; a size above the receive limit
  is ignored. A
  pointer is taken only when it can be checked (it states a digest, or the attachment has a
  plaintext hash among its names) and never replaces the working pointer of a file that is
  there; a pointer without a digest is checked against the plaintext hash, and one with a
  digest against it too when there is a hash (`Integrity::Both`: an answer matched to the
  wrong attachment is refused, not shown under it). The new pointer (v2) replaces the old one
  and its names are added; `attachments.restored` marks it. Only the attachments this device
  asked for (`Asking`) download, with source `restore`; answers to another device's request
  (the phone sends every snapshot to all our devices) give lost attachments a pointer and
  download nothing. A final answer ends the request: the attachment a "gone" slot was placed
  on becomes "Not on your phone either." (`NoFile`, final), one still asked for that no slot
  placed or whose pointer could not be used becomes "did not answer" (clickable again: a
  matching gap of ours is no statement about the phone); `MESSAGE_NOT_FOUND` ends them as
  "Your phone no longer has this message." (`NoMessage`, final). A download the USER started
  that ends `Expired` asks at once (`attachment_downloads.source` 'user'); an automatic one
  asks nothing, and one whose pointer came from an answer does not ask again by itself.
  Tests: `kourier-core/tests/restore.rs`, `restore_fake.rs` (the fake's `BackfillScript`
  knob; the row's per-attachment list, a number-only chat), `restore_start.rs` (a real core
  started on the files of an earlier run: the orphan ask ended at the start, the first look
  before the actor is there, the hold from the spawn, the end of the hold when online; each
  fails when its line of wiring is taken out), `kourier-signal/tests/backfill.rs`
  (mock: the request as the phone sees it and its urgency, answers in steps, not found, a
  pointer without a digest and one with a digest and another file's hash, a request that
  outlived its hour before a socket came up, an answer across a restart), unit tests in
  `backfill.rs`, `maintenance.rs`, `receive/tests.rs` (a request's answer finds its message
  again for every shape of chat and author), `policy.rs`, `actions.rs`, the viewmodel's
  `timeline.rs`, and `kourier-qt/tests/ask_ui.rs` (the Download on a file, a picture, a
  sticker's stand-in and the viewer, and the block turning to "Asking your phone…"). Known
  limits: an edited message takes no long text from an answer; an iPhone primary was never
  tried (it may not answer: the note covers it); an answer that arrives after the wait ended
  gives the file a pointer but starts no download (its note says the phone did not answer,
  and a click downloads); the waits' lengths are process-wide (`tune_waits`).

### 6.12 Maintenance jobs

Every job follows §5.6: network first, then one zone that applies the result and inserts the
implied core events into `core_outbox`. Jobs live in the `jobs` table and survive restarts. A
job that failed with a rate limit waits at least as long as the server's Retry-After. A job
that meets a 428 challenge keeps the token, is marked `challenged` and announces
`BackendNotice::ChallengeRequired` once while that challenge stands. Every other queued job
that sends to another account (receipts, delivery receipts, retry requests, resends, timer
updates) is held meanwhile without a request of its own, its previous due time kept in
`held_from`; a job queued later is held when it comes up.

Jobs run one at a time, by priority, except profile fetches: up to 10 due `profile_fetch`
jobs run together (`jobs::PROFILE_FETCH_BATCH`; Desktop's `ProfileService` runs 30), and
the next job starts when the whole batch has ended. A history import or a first Storage
Service sync queues one fetch per contact. One after another they took many minutes on
the user's account (2026-10-10). A 413 or 429 on a profile fetch holds every queued
profile fetch until its Retry-After, or one minute without one, as Desktop pauses its
queue. A batch logs one `jobs done` line, not one line per profile. After every batch, and
whenever no job is due, the runner looks at the post-link gate (§6.5 step 8), so the jobs a
link queues never hold its `Done`. An accepted `SubmitChallenge` makes
the job that met the challenge due at once and gives each held job its own time back (now, if
that has passed), so a rate limit or backoff of a held job is never cut short. A challenge
met by a message send does not hold jobs.

A challenge hold never outlasts the notice it was announced with. It ends at the accepted
answer, at the challenge's Retry-After, at a fresh `Online` that ends the core's notice (§4.8
step 3), and at the first `Online` after a restart. The standing challenge and its token are
in memory and the marks in `backend.db`, so at those two kinds of `Online`
`Shared::forget_job_challenge` gives the held jobs their own times back. The first of them
asks the server once; a challenge that
still stands is announced again with a new token and holds the rest again. A challenge
announced during a catch-up stands through the `Online` that ends that catch-up, as the core
counts it (`challenge::Standing::at_online`). The Retry-After of a challenge is therefore not
waited out across a reconnect or a restart, where Signal Desktop keeps waiting for the
server's time: a restart loses the token, and a notice the core shows again could not be
answered otherwise. The cost is one refused job request and one new notice per fresh
`Online` while a challenge stands.

| Job | When | What |
|---|---|---|
| Forwarder | always (long-running task) | `core_outbox` → core, §5.6 |
| Key maintenance (`key_maintenance`) | at start, every 2 days (first run 2 days after a link, 5 min retry after a failure), forced for PNI after `PniChangeNumber`; one-time counts also at every connect | `GET /v2/keys` counts; refill a kind with a batch of 100 if < 10 or > 200; rotate signed + Kyber last-resort if the newest confirmed one is older than 1.5 days; one `PUT /v2/keys` with only what it brings; keep the newest 5 signed and last-resort keys and delete older ones past the server's queue time (≥ 30 days), one-time keys beyond the newest 200 after 90 days |
| Timer update (`timer_update`) | after our own 1:1 timer change, while Online, retried for a day | the `EXPIRATION_TIMER_UPDATE` message to the peer and its sync transcript (§6.13) |
| Capabilities | every start | `PUT /v1/devices/capabilities` |
| Remote config | connect + every 2 h | §6.4 |
| Sender certificates | before expiry − 1 h | both variants |
| MSL / `send_progress` / `sent_quotes` purge | hourly | older than 14 days |
| Parked lanes | backoff 5 s … 10 min | resume stage 2 for the lane |
| Dirty groups | backoff 5 s … 10 min, and on next message | refetch state/logs (§6.8) |
| Group credentials | when window < 2 days left | refetch |
| Receipts, read syncs, delivery receipts | after catch-up; retried on transient errors | §4.8, §6.7 |
| Delete sync (`delete_sync`) | after `Command::ClearConversation` or `DeleteAttachment` | `SyncMessage.deleteForMe` to our own devices (§6.13); the payload is the message without padding, so a resent command finds its job or its done record |
| Profiles | §6.10 | |

### 6.13 Mapping Signal → core events

All core events below leave the backend through `core_outbox`, committed together with the
backend-side handling (§5.6).

| Signal input | Backend handling | Core event(s) |
|---|---|---|
| `DataMessage` with body/attachments/quote/sticker/contact | validate, profile key side effect; a long text is downloaded first (§6.11) | `Message` |
| `DataMessage` with `isViewOnce` (also in `SyncMessage.sent`) | no attachment mapped, raw not kept (§6.11) | `Message(Unsupported "view-once")` with `view_once` set |
| `DataMessage.reaction` | the emoji must be one emoji by shape, else the reaction is dropped (the other clients drop it too); with `storyContext` it is no reaction but a placeholder | `Reaction`, or `Message(Unsupported "story-reply")` |
| `DataMessage.delete` / `adminDelete` | — (author/admin and window checked by the core, §3.8) | `Deleted{for_everyone}` |
| `EditMessage` | an edit with attachments is the new caption (`Body::Media{caption}`, none = removed; the core keeps the files); an edit with a long text waits for its download (retried, 10 attempts). An edit that is itself a view-once, contact-share or voice message is dropped (every official client drops it). A remote edit is dated with the envelope's server timestamp. Author, conversation, whether the target can be edited and the 48 h window are checked by the core | `Edited{new_local}` |
| flags `EXPIRATION_TIMER_UPDATE` (1:1), and the timer every 1:1 `DataMessage` states for its chat | version rules (§13 phase G) | `ConversationUpsert{timer, timer_version}` + `Message(System TimerChanged)` |
| flags `PROFILE_KEY_UPDATE` | store, maybe refetch profile | (later `ContactUpsert`) |
| flags `END_SESSION` | archive sessions | `Message(System SessionRefreshed)` |
| `ReceiptMessage` | — | `Receipts{conv: None}` |
| `SERVER_DELIVERY_RECEIPT` envelope | — | `Receipts{Delivered}` |
| `TypingMessage` | setting + membership checks | `Typing` |
| `SyncMessage.sent` | recipients' states | `Message{origin: OwnOtherDevice}` / `OwnSendState` |
| `SyncMessage.read` / `.viewed` / `.viewOnceOpen` | — | `ReadMarker` / `Receipts{by: own ACI, Viewed}` |
| `SyncMessage.blocked` | — | `ContactUpsert{blocked}`, `ConversationUpsert{blocked}` |
| `SyncMessage.configuration` | — | `AccountSettings` |
| `SyncMessage.contacts` | download blob | `ContactUpsert` × n |
| `SyncMessage.messageRequestResponse` | — | `ConversationUpsert{request_pending, blocked}` |
| `SyncMessage.deleteForMe` | `messageDeletes` per message; `conversationDeletes` with anchors `mostRecentMessages` then `mostRecentNonExpiringMessages` and `remove = isFullDelete`; `localOnlyConversationDeletes` without anchors and `remove` set; `attachmentDeletes` with the names `uuid:<hex>` and `digest:<hex>`, which incoming attachments carry as `net_ids`. Entries named only by phone number or only by plaintext hash are left out | `Deleted{for_everyone: false}`, `ConversationCleared`, `AttachmentDeleted` |
| `Command::ClearConversation` / `DeleteAttachment` (deletes made in Kourier) | `conversationDeletes` with `isFullDelete = remove`, `mostRecentMessages` newest first (at most 5) and `mostRecentNonExpiringMessages` only when it differs; no anchor and `remove` → `localOnlyConversationDeletes`; no anchor and no `remove` → confirmed, nothing sent. `attachmentDeletes` with `clientUuid` and `fallbackDigest` from `net_ids`; without either, confirmed and nothing sent. A group is named by `threadGroupId`, a 1:1 chat by `threadServiceIdBinary` (the ACI, or the PNI for a chat known by number), a message by `authorServiceIdBinary` + `sentTimestamp`. Signal Desktop always sends `isFullDelete = true`; Kourier sends false for "clear" and true for "delete". On Desktop, Android and iOS false empties the chat up to the anchor and keeps it, and only true goes on to remove the chat (sources in §13 phase J; Android sends false itself when it trims a chat). The field is always present on the wire: iOS drops an entry whose flag is unset. An entry without `mostRecentMessages` is dropped by Desktop and finds no anchor on the phones | `CommandPersisted` |
| `SyncMessage.keys` / `.fetchLatest` | update keys / trigger sync | — |
| `SyncMessage.request` / `.verified` / call events | ignore (calls out of M1) | — |
| Storage Service records | merge | `ContactUpsert`, `ConversationUpsert{…, marked_unread}`, `PinnedConversations`, `AccountSettings`, `IdentityMerged` (number → ACI only when `number_links` did not hold that pair yet, so a record that comes back, `storage::reset`, tells nothing; a record whose previous version stated the number was told before schema 21 and is recorded silently) |
| Group state change | fetch/apply | `ConversationUpsert` + `Message(System …)` |
| Identity key changed (decrypt or profile fetch) | archive sibling sessions, downgrade verified | `ContactUpsert{fingerprint, trust}` + `Message(System IdentityKeyChanged)` |
| A PNI and an ACI learned to be one contact: a Storage Service contact record with both ids, a verified PNI signature, or a sealed sender's certificate that states for an ACI the number a contact record gives to a PNI without an ACI (nothing joins when two PNIs hold the number) | recorded in `identity_links` by one funnel (`links::learn`); a rebuilt core gets every pair again. Not a source: the group change that promotes an invited PNI to its ACI (only the server vouches for it, and a merge moves a 1:1 chat) | `IdentityMerged`, once per pair |
| Message addressed to our PNI | set `share_my_number` | — |
| Undecryptable (Resendable) | retry request | `Message(Placeholder Undecryptable)` |
| `DataMessage` content Kourier does not show: contact card, payment, gift badge, poll, poll end, story reply, group call update, pin, sticker without an image, `requiredProtocolVersion > 8` | raw DataMessage kept for a later version | `Message(Unsupported)` with `kind_hint` `contact`, `payment`, `gift-badge`, `poll`, `poll-ended`, `story-reply`, `group-call`, `pin`, `sticker`, `newer-protocol` |
| `StoryMessage`, `CallMessage`, poll votes, unpins | drop | nothing |

### 6.14 Linked-device obligations (R:signal-desktop-receive §9) → where and when

| Items | Module | M1 step |
|---|---|---|
| 1-3 provisioning, URL, atomic link, capabilities | `provisioning.rs` | S2, S6 |
| 4 prekey upload and maintenance, capabilities on start | `keys.rs` | S2, S15 |
| 5 identities as provisioned, storage key | `provisioning.rs`, `store/` | S2 |
| 6 sync requests | `provisioning.rs`, `actor.rs` | S6 |
| 7 Storage Service as source of truth | `storage.rs` | S9 |
| 8 one auth connection, 4409, 401/403, wipe on unlink | `net/`, core lifecycle | S6, S15 |
| 9-11, 29 ack after commit, ordering, all envelope types, durable inbox | `receive/`, `zone.rs` | S2, S7 |
| 12 sealed-sender validation | `receive/` | S7 |
| 13 PNI rules, PNI signature, change number | `receive/`, `send/`, `keys.rs` | S7, S8, S15 |
| 14-15 identity changes, Kyber triples | `store/` | S2, S7 |
| 16-17 retry both directions, MSL | `retry.rs`, `msl.rs` | S8, S11 |
| 18-19 validation, dedup | `receive/` + core ingest | S7, S13 |
| 20 group membership rules | `receive/`, `groups/` | S10 |
| 21-23 sync messages, pending modifiers | `receive/` + core `pending_modifiers` | S8, S13 |
| 24 receipts policy | `send/` | S8 |
| 25 typing validation | `receive/` | S8 |
| 26 edit/delete rules | core features + ingest | S13 |
| 27 pause side effects while catching up | `actor.rs` + core notification policy | S7, S14 |
| 28 keepalive, backoff, clock skew | `net/` | S15 |
| 30 encryption at rest, key in Secret Service | `kourier-db`, `kourier-blob`, `kourier-app`, `kourier-desktop` | S1 |

### 6.15 libsignal upgrade procedure

- Track the libsignal version Signal Desktop's `main` uses, or newer; expect a bump about every two
  weeks (5-11 tags a month, 17 breaking minors in 8 months, R:libsignal §5).
- Per bump: read the tag's release notes (`git tag -n80 vX`), diff `rust/bridge/shared/**` for
  policy changes (session usability flags `NotStale | EstablishedWithPqxdh | Spqr`, capabilities,
  remote-config keys), diff Desktop's `ts/types/Capabilities.d.ts` and protos, update all libsignal
  crates to the same tag, run unit tests, `FakeChatRemote` tests (`libsignal-net` feature
  `test-util`) and the mock-server suite.
- Keep the libsignal crate set minimal: never `device-transfer`, `libsignal-bridge*` (they pull
  crates.io `boring`).

---

## 7. How Threema and WhatsApp plug in later

### 7.1 WhatsApp (whatsapp-rust, R:whatsapp)

| Flow | Through the contract |
|---|---|
| Crate | `kourier-whatsapp`, whatsapp-rust pinned by git rev, `default-features = false` (no `sqlite-storage`; keep `tokio-transport`, `tokio-runtime`, `ureq-client`); implements whatsapp-rust's `Backend` store traits over its `backend.db`. The async store calls go over a channel to a dedicated per-account DB thread that owns the connection; blocking rusqlite never runs on the shared runtime, which is also whatsapp-rust's read loop (R:whatsapp §3.5) |
| Linking | `StartLogin{"qr"}` → `ShowQr` every 60 s then 20 s (`WA/src/pair.rs:115-117`) → `Done`; `StartLogin{"pair-code"}` → `Input{phone}` → `ShowCode{8 chars}`. `risk_notice` shown first |
| History | `HistorySource::PushedOnLink`: `Event::HistorySync` → `HistoryBatch` (≤ 500 messages per `core_outbox` row; silent; `sort_key` from `sent_at`; `initial_out_state`, `read`, `reactions`; per-chat unread → `unread_hint`); later PDO → `FetchHistory` (`OnDemand`); newsletters per conversation |
| Receive: E2E messages | inline `EventHandler` (never the default closure delivery, which drops events on overflow, `WA/src/bot/event_delivery.rs:37-59`); `InboundDurabilityHook` commits the batch to the backend inbox and returns Ok → stage 2 and forwarder as Signal; `Messages` → `Message` |
| Receive: everything else | App-state mutations, receipts, group notifications, contact updates and history results arrive as events *after* whatsapp-rust has persisted its cursor through our store traits (`WA/src/client/app_state.rs:2283-2292`: the version is stored before return, a dropped collection is "lost for good"), and `EventHandler::handle_event` is synchronous (`WA/wacore/src/types/events.rs:404-407`). Adapter rules: (a) `handle_event` translates the event and inserts it into `core_outbox` through the DB thread (`block_in_place` + wait for the commit) before it returns; (b) our `AppSyncStore::set_version` writes `appstate_dirty(collection, version)` in the same transaction as the version, the flag is cleared in the transaction that puts that collection's mutations into `core_outbox`, and a dirty collection at startup triggers a full snapshot resync (`WA/src/features/app_state_resync.rs`); (c) PDO-recovered messages and newsletters bypass the hook (`WA/src/types/durability_hook.rs:40-49`) and are at-most-once: accepted and documented. Mapping: `Receipt` → `Receipts` per participant; app state → `ConversationUpsert{archived, pinned, muted, marked_unread}` / `ReadMarker` / `Deleted{for_me}`; LID↔PN → `IdentityMerged` (+ `merge_conversation`) |
| Catch-up | `Syncing` until `OfflineSyncCompleted` and the boundary rows are acked by the core (§4.5) |
| Send | `allocate_message_id` returns only the local id in whatsapp-rust's format (`RequestUtils::message_id_at(pn, secs)`, `WA/src/request.rs:268-280`); own messages are stored with `author: None`, matched per conversation, so PN vs LID addressing never matters; `Transmit` uses `SendOptions::with_message_id`; nack 463 → `Failed{retryable: false}` with a user message, never auto-retried |
| Roaming state | `ChangeConversation` → app-state mutation → backend emits `ConversationUpsert` after the server confirms |
| Presence | reads `UiPresence` (focus, visible chat): available only while the window is focused; chatstate subscriptions for the visible chat |
| States | `TemporaryBan` → `Restricted`; `StreamReplaced` → `NeedsUserAction(Reconnect)`; `ClientOutdated` → `NeedsUserAction(UpdateApp)`; `LoggedOut` → `LoggedOut` |
| Media | `DownloadAttachment` with directPath+mediaKey blob; expired media → `AttachmentDone{Err(Expired)}`; re-upload request is a later `Command` |

### 7.2 Threema (libthreema + ported Threema Desktop logic, R:threema)

| Flow | Through the contract |
|---|---|
| Crate | `kourier-threema` with vendored libthreema (path dependency, no features) and the `[patch.crates-io] blake2` it needs in our root `Cargo.toml`; own thread + LocalSet (libthreema state machines are `Rc<RefCell>`), sans-IO driver feeding the mediator WebSocket |
| Linking | `ShowQr{threema://device-group/join#…}` → `ConfirmEmoji{rendezvous path hash}` → `Input{password}` (only if we keep Threema's own key-file model; with Secret Service we skip it) → `Progress("Importing contacts and groups")` → `Done`; EssentialData → `ContactUpsert` (with `trust`, `hidden`), `ConversationUpsert` (groups keyed creator+id, distribution lists as `Broadcast`) |
| Receive (follower) | reflected envelope → one `backend.db` transaction: inbox row + D2D nonce, and for `ContactSync`/`GroupSync`/`SettingsSync` reflections the applied state *plus* its events in `core_outbox` (R:threema §5 item 5) → `reflected-ack` → stage 2 / forwarder |
| Receive (leader) | CSP message → decrypt → reflect to own devices → await reflect-ack → commit inbox + CSP nonce → CSP `message-ack` → stage 2 / forwarder; `ConnState::Online{detail: "leader"}` |
| Catch-up | `Syncing` until `ReflectionQueueDry` and the boundary rows are acked by the core (§4.5) |
| Ids | `msg_id_scope: Conversation`; `local` = u64 hex; reactions, quotes and receipts carry only the u64, so the backend emits `TargetAuthor::Unknown` and the core resolves within the conversation (as Threema Desktop's `getMessage(messageId)`, `TD/apps/desktop/src/common/network/protocol/task/common/message-reaction.ts:56`) |
| Send | `allocate_message_id` = random u64; Transmit = reflect `OutgoingMessage` → per-receiver CSP sends (honouring `only`, recording accepted receivers like `send_progress`) → reflect `Sent` update; groups are client-side fan-out and may end `Partial`; distribution lists send 1:1 copies and emit per-member `Message{linked_local}` (§4.2) |
| Roaming state | every contact/group/settings/profile change is a D2M transaction inside the backend (`ChangeConversation`, `ChangeContact`, `ChangeAccountSettings`, `ChangeProfile`); the core applies it only from the resulting events (principle 4.1.4) |
| Features | 6-hour edit/delete windows, receipts 1:1 only, typing 1:1 only, `reactions_per_user: None` (set), mentions/quotes as inline markup parsed by the backend |

### 7.3 What the M1 contract already contains for them (so the core does not change)

| Need | Contract item |
|---|---|
| Crash-safe state mirroring | transactional-outbox rule, `core_outbox` + forwarder (§4.1, §5.6) |
| Conversations | `ConvKey::{Broadcast, Channel}`, `ConvKind` beyond Direct/Group, `ConversationPatch.{parent, marked_unread, unread_hint}` |
| Login | `LoginStepKind::{ShowCode, ConfirmEmoji, Input}`, repeated `ShowQr`, `StartLogin.relink` |
| History | `HistoryBatch`, per-conversation `HistorySource`, `FetchHistory`, `history`/`silent`, `IncomingMessage.{initial_out_state, read, reactions}` |
| Identity and references | structured `NetMsgId`, `msg_id_scope`, `TargetAuthor::Unknown`, `IdentityMerged`, `identity_alias`, `merge_conversation` |
| Catch-up | `IncomingMessage.catch_up`, the boundary rule (§4.5) |
| Features | `reactions_per_user: None`, per-conversation receipt/typing flags, receive windows, `group_actions`, `cannot_send` |
| Roaming changes | `ChangeConversation`, `ChangeContact`, `ChangeAccountSettings`, `ChangeProfile` with replies; `ListOwnDevices` |
| Sending | `TransmitOutcome::Partial`, `OutgoingRequest.{only, is_recipient_update}`, broadcast sends with `linked_local` |
| States, presence, media | `ConnState::{Restricted, NeedsUserAction, Online{detail}}`, `ErrorKind::RateLimited`, `UiPresence`, `AttachmentState::Expired`, opaque `remote` blobs, `BlobTarget` streaming |
| Contacts, consent | `ContactPatch{trust, hidden, identifiers}`, `TrustLevel`, `risk_notice` |

The kourier-fake conformance suite carries one script per future network before the contract is
frozen in S3 (R:multi-protocol-prior-art §6): WhatsApp history shapes (own messages with states,
reactions, unread hints, PN→LID merge with both chats populated), Threema references with
`TargetAuthor::Unknown`, a rejected Threema transaction, a distribution-list send.

Additions expected later, all additive (new enum variants or commands, no schema rewrite):
polls and location rendering, `RequestAttachmentReupload` (WhatsApp), device drop (Threema), work
flavours/OnPrem config in `BackendConfig` (Threema).

### 7.4 Policy gates

- Threema: the ToS (2022-03-23, §2.1, §4.2) allow operational use only with official or approved
  clients; §4.3 allows modified open-source clients for testing (R:threema §6). The backend stays
  unbuilt until the user decides.
- WhatsApp: the ToS forbid unofficial clients; bans and companion removals are reported in 2025-26
  (R:whatsapp §5.1). If built: opt-in only, `risk_notice` with explicit consent, WA-Web-parity
  device props, no auto-resend after 463, behind a cargo feature and a runtime "experimental" flag.

---

## 8. UI layer

### 8.1 Crate and bridge

- `kourier-qt` is the only crate with cxx-qt (0.10) and the only QML module (URI
  `app.kourier`). Bridges are thin; logic lives in `kourier-viewmodel`.
- `App` is a `#[qml_singleton]` with `Threading`. It owns sub-objects as `CONSTANT` `*mut QObject`
  properties: `accounts`, `conversations`, `timeline`, `composer`, `login`, `settings`. Its
  `Initialize` takes the `UiContext` (AppHandle, event receiver, media reader) from a
  `static OnceLock` set by `main` before `engine.load()` (R:qt-kirigami §3.2).
- Models are `#[base = QAbstractListModel]` Rust QObjects with rows of prebuilt `QString`/`QUrl`
  values; a safe `apply(ListOp)` wrapper pairs `begin*/end*` calls.

### 8.2 Startup sequence

1. `main`: parse args (`--background`, `--profile`, `--fixture`, `--page <name>`, `--screenshot=<png>`,
   `sgnl://…` URLs); set umask 077; single-instance check over D-Bus (`kourier-desktop`): if the name
   `app.kourier.Kourier` exists, call `org.freedesktop.Application.Activate`/`Open` with
   `$XDG_ACTIVATION_TOKEN` and exit 0.
2. `kourier_app::start()`: build the tokio runtime (`Builder::new_multi_thread`, not
   `#[tokio::main]`), load the master key (Secret Service or passphrase prompt — the passphrase dialog
   is QML, so this step can complete after step 3), open `core.db`, start the core, register networks,
   do **not** connect backends yet.
3. `QApplication::new()` (needed by the `org.kde.desktop` style), `set_desktop_file_name("app.kourier.Kourier")`,
   style `org.kde.desktop` unless `QT_QUICK_CONTROLS_STYLE` is set, load
   `qrc:/qt/qml/app/kourier/qml/Main.qml`.
4. First `frameSwapped` → `App.ready()` → core sends `Connect` to all accounts.
5. Shutdown: cancel producers → `exec()` returns → drop engine → core `Shutdown` with timeout →
   `runtime.shutdown_timeout(5 s)`.
6. Hidden to tray for 60 s → release the root window in two steps (engine and models stay).
   At the release the shell reports presence (not focused, no chat on screen), calls
   `App.windowReleasing()`, which sends `UiCommand::ReleaseMemory` (the core lets go of the
   page caches and prepared statements of its connections and reads them again when it is next
   asked something; the call does not wait for it), and calls `destroy()`, which only
   schedules the deletion. When the window object is really gone (a 50 ms timer polls for
   it), the shell runs `gc()` and `App.windowReleased()`, which calls `malloc_trim(0)` and
   nothing else: it must never touch presence, because an activation can fall between the two
   steps and its new, focused window would be counted as absent. A second trim runs 2 s later
   (`RELEASE_SETTLE`), because the core frees its caches on its own threads after the queries
   and writes that were running, and the event loop goes on deleting what the window owned.
   Activation recreates the window. A trim also runs 5 s after the first frame
   (`STARTUP_TRIM_AFTER`).

### 8.3 QML structure (Kirigami)

```
crates/kourier-qt/qml/
  Main.qml                    Kirigami.ApplicationWindow; states: noAccounts | ready; wideScreen ⇒ 2 columns
  AccountsDrawer.qml          Kirigami.GlobalDrawer (collapsible sidebar when wide, modal when narrow):
                              "All chats", one entry per account (avatar, network icon, state dot),
                              "Add account", "Settings"
  pages/WelcomePage.qml       first run: choose a network → LinkAccountPage
  pages/LinkAccountPage.qml   generic LoginStep renderer: Prison Barcode (QR), code, emoji row,
                              FormCard inputs, progress, error with retry; risk notice dialog first
  pages/ConversationListPage.qml  Kirigami.ScrollablePage + ListView(App.conversations);
                              RoundedItemDelegate rows: Components.Avatar(image://kourier/avatar/…),
                              title, preview, time, unread pill, muted/pinned icons, network badge
                              (only when several accounts); SearchField filters in Rust; sync banner;
                              components/AccountBanner.qml on top (offline, link again, verify, clock,
                              reconnect); the composer's notice when the composer cannot show it
  pages/ChatPage.qml          ONE instance, created when the first conversation opens (a cheap
                              stand-in page holds the chat column of a wide window until then);
                              App.timeline.conversationId switches the model;
                              header: avatar, title, typing subtitle, timer menu (with "Custom
                              Time…"), "Clear Chat History…" and "Delete Chat…"; footer: Composer
  timeline/TimelineView.qml   ListView BottomToTop (row 0 = newest), DelegateChooser on `kind`
                              (Message, DateHeader, UnreadMarker, System, Loading), paging by distance
                              to the ends, "Jump to Latest" button parented to the ScrollView and shown
                              by distance from the newest message; holds on to the newest message
                              or, once the user moved the view away, to the row at its top while
                              rows change (§13 phase M); read tracking, keyboard focus
                              (the timeline is one tab stop; Up/Down move; Enter or Space
                              activates the current row's picture, file or voice note where the
                              block names an action, else opens its menu; Ctrl+Enter goes to the
                              quoted message; Menu or Shift+F10 always opens the menu)
  timeline/MessageDelegate.qml  bubble; author line (showAuthor from model); Repeater over `blocks`;
                              toolbar (React, Reply, More); reaction chips below the bubble
  timeline/SystemRow.qml      notices; context menu with "Delete…" where the row's actions allow
  timeline/{MessageMenu,ReactionPicker,ReactionsPopup}.qml   context menu built from the row's
                              `actions` role (singleton TimelineActions.qml), reaction picker
                              (created on first use), who-reacted list; the menu also has the
                              row's media action, "Go to Quoted Message" and "Delete Attachment…"
  timeline/blocks/{Text,Image,Album,File,Voice,Quote,Sticker,Placeholder,Unsupported}Block.qml
                              TextBlock = read-only TextEdit (RichText from Rust, selectByMouse,
                              onLinkActivated → confirmation) + spoiler overlay rectangles;
                              AlbumBlock = grid of the neighbouring pictures and videos of one
                              message (2 to 5 tiles, "+N" beyond five)
  composer/Composer.qml       QQC2.TextArea (Sonnet spell check from the style), Enter sends,
                              Shift+Enter newline, attach (a menu "Files…" and "Folder…" over
                              composer/AttachDialog.qml), DropArea, paste (files, folders or an
                              image); a folder is a row of its own in the list while it is
                              counted, asked about (dialogs/FolderQuestionDialog.qml) and then
                              replaced in place by its files or by one .zip, with a progress
                              bar while it is packed (below); the waiting files are a list
                              (App.composer.files: any number, each with size, its message number
                              or the reason it cannot go, a summary line "46 files, 19 messages"),
                              kept per chat for the session; a picture or video row has a "send
                              as original file" toggle, and a quiet note marks a file type some
                              client of the network will not save (features.hard_to_save);
                              emoji popup, reply/edit banner; disabled with reason from features.cannot_send
  composer/EmojiPopup.qml     popup around composer/EmojiGrid.qml: category tabs and a grid over the
                              static singleton Emoji.qml (there is no Rust EmojiModel yet); the
                              reaction picker uses the same grid
  settings/*.qml              FormCard pages via pageStack.pushDialogLayer: General (tray, autostart,
                              compact layout), Notifications, Privacy (shows synced read-receipt/typing
                              settings read-only in M1), Accounts (linked device info, unlink), About.
                              As built it is one file, settings/SettingsPage.qml, with the groups
                              General, Notifications, Privacy, Disappearing Messages (three radio
                              buttons for the core's expiry option, §8.9) and About
  dialogs/{ImageViewer,DeleteDialog,ChallengeDialog,RevisionsDialog,TimerDialog,ChatDeleteDialog,
           AttachmentDeleteDialog,FolderQuestionDialog}.qml   created lazily; the viewer steps
                              through a whole album and
                              follows its message's row (a picture that arrives or is deleted);
                              RevisionsDialog is the edit history behind the "edited" mark and
                              refreshes while open; TimerDialog takes a custom time inside the
                              chat's range and, for a time that is off the range's step, names
                              the nearest times that can be set; the two delete dialogs say
                              what happens on the other devices; FolderQuestionDialog asks what
                              to do with the folders in the composer; SafetyNumberChanged,
                              RiskNotice and Passphrase are not built yet
```

A folder in the composer (user decision, 2026-10-07; `kourier_app::folders`,
`kourier_viewmodel::files`). A folder cannot travel as a message, and it is never marked
"Not a file" (that reason is left for sockets, pipes and devices). Dropped, pasted or picked,
it stays in the list where it was put, as a row of its own, while a worker thread counts what
is inside. Then one dialog asks, once for all the folders that wait in that chat: it says how
many files they hold, their size and what a walk leaves out, and offers two answers.

- "Add the Files Inside": the folder's files take the row's place, ordered by path, and are
  sent like any picked files. Above 1,000 files (`files::LARGE_ADD`) the dialog asks once
  more; after the second yes every file is added.
- "Send as One .zip" ("Send as N .zip Files" for several folders): each folder is packed
  into one archive off the Qt thread, one after the other, with a progress bar in its row;
  removing the row stops the packing. The archive then takes the row's place as a plain file.
  This answer is offered only when the chat takes plain files and each folder's files add up
  to no more than the chat's limit for one (`features.files[File].max_size`, the estimate of
  the archive's size, checked before anything is packed); otherwise the dialog says why.

The walk is the same for the count, the list and the archive: every regular file below the
folder, subfolders too, in an order by path that compares names byte for byte. Left out and
counted for the dialog: names that start with a dot, files and folders alike (the folder that
was given is taken whatever its name); symbolic links, which are never followed and never
read (the given folder may itself be one); sockets, pipes and devices; files and subfolders
that cannot be looked at. A file that goes away during the walk is simply absent. The archive
is a ZIP with deflate, one entry per file named `<folder>/<path below it>`, never absolute and
never with a `..` part, with each file's modification time and permission bits and no owner.
Where it is written and when it is removed is §5.8; what the packing may open is §9.4. The
question for a chat that is not open waits until that chat is opened.

Unified inbox is the default ("All chats"); selecting an account filters the list. The composer
always sends through the conversation's own account.

### 8.4 Rust models and the viewmodel crate

| Model | Rows / roles | Source |
|---|---|---|
| `AccountListModel` | name, network, avatar, conn state, unread | `UiEvent::AccountsChanged` + queries |
| `ConversationListModel` | title, preview, time, unread, mentions, muted, pinned, avatarUrl, accountBadge | query by filter, read in pages of 500 to the end (no cap on the number of chats; nothing half-read is shown); invalidations patch single rows; sort pinned → last activity in Rust (`beginMoveRows`) |
| `AttachmentListModel` | fileId, name, kind, icon, preview, size, reason, part, partFirst, partLast, stage, progress | the composer's waiting files and folders (`kourier_viewmodel::FileStrip`); the split and the refusals come from `Query::FilePlan`, which is asked about the entries that are not folders (§8.3); `stage` is a folder row's `counting`, `asking`, `listing` or `packing`, `progress` its packing from 0 to 1 |
| `TimelineModel` | kind, blocks (QVariantList), author, showAuthor, showDateHeader, time, outState, edited, reactions, replyPreview, actions (what the core allows for the row, §3.8) | windowed (≤ 500 rows), keyset paging, diffs from `kourier-viewmodel` |
| `MemberListModel`, `EmojiModel` | | |
| `LoginController`, `ComposerController`, `SettingsController` | properties + invokables | commands |

`kourier-viewmodel` (no Qt) does row building, grouping flags (same author within 5 min ⇒ no author
line; date headers), preview text, relative time formatting, `RangeKind` → message RichText, UTF-16
conversion for the composer, and the diff engine producing
`ListOp::{Reset, Insert{at,n}, Remove{at,n}, Move{from,to}, Change{at, roles}}`. It is tested with
plain `cargo test`.

**The line under a message on its way** (link feedback L6). An own message that has not left
three seconds after its `sent_at` (`timeline::WAIT_SHOWN_AFTER_MS`; normal sends stay clean) gets
a line under its bubble, built in `kourier-viewmodel` (`timeline::wait_line`) in two labels: the
REASON (it may be cut short in a narrow window) and the TIMER (never cut, a label of its own,
`wait` and `waitTimer` roles). The timer counts up from the message's stored `sent_at`, so it
survives a restart. The reason is the real hold, in the words of the banner above the chat that
says the same: waiting for the connection (offline, connecting), for the check ("Verify…"), for
the new safety number to be confirmed (the queue behind a paused head), for the clock, for the
device to be linked again, the network's restriction, an update, a reconnect; or, while the
connection is up, the step of the running attempt (`SendPhase`: uploading files, checking the
group, setting up encryption with N members, sending to N members, "Sent to M of N members",
updating the members' devices, sending to your other devices) or plain "Sending…". While the
outbox waits for a retry the line is the countdown from the stored `next_attempt_at` ("Next try
in 0:24", rounded up; a hold beats it). The per-second tick brings up to date only the rows that
show a line (`Changes.waits`, `UiSource::wait_lines`, `models::patch_row`: the `wait` and
`waitTimer` roles of those rows), never the whole timeline; so does a step report
(`Progress::Transmit` with a phase, up to four a second in a group send; `None` ends the step
shown, so the line falls back to "Sending…"). The row's accessible description carries the
reason without the seconds, ending in one stop (none after a reason that ends in "…" or ".").

**Message text (decision).** Selectable formatted text needs `TextEdit` (Kirigami's
`SelectableLabel` is one: `org/kde/kirigami/controls/SelectableLabel.qml:22` in the system QML
dir), and `TextEdit` has no `StyledText` (only Plain, Rich, Auto, Markdown). So the text block is a
read-only `TextEdit { textFormat: TextEdit.RichText; selectByMouse: true }` fed with HTML that
Rust generates from `Text` ranges using a fixed whitelist: `<b> <i> <s> <code>` and `<a href>`
with schemes `http`, `https`, `sgnl` only; every text run is HTML-escaped; no `img`, `style`,
`font` or attributes besides `href`. Network-provided markup never reaches Qt. `onLinkActivated`
opens a link only after a confirmation. Spoilers: the viewmodel passes their UTF-16 ranges; QML
draws opaque rectangles over them (from `positionToRectangle`) until clicked. S4 measures the
per-delegate cost in the scroll test; the fallback is `Text` + `StyledText` with a per-message
"select text" mode.

### 8.5 Threading between tokio and the Qt thread

- UI → core: invokables call `AppHandle::send(UiCommand)` (non-blocking). Results come back as
  events. No DB access and no `block_on` on the Qt thread (clippy-enforced).
- Queries: `UiCommand::Query{token, kind}` is answered with `UiEvent::QueryResult{token, rows}`
  computed on a core read connection. Two queries for the same model can finish out of order on the
  two read connections, so each model keeps one query in flight: invalidations that arrive
  meanwhile are coalesced into the next query, and every token carries the model's monotonically
  increasing generation; a result is applied only if its generation is ≥ the last applied one
  (stale conversation tokens are dropped the same way). A viewmodel test completes queries in
  shuffled order. Later option for the open timeline: diffs pushed by the core writer after commit
  (matrix-rust-sdk `VectorDiff`) instead of re-queries.
- Core → UI: one channel of `UiEvent` (invalidations such as `ConversationsChanged{ids}`,
  `MessagesChanged{conv, ids}`, `ConversationMerged{from, into}` (§3.3), plus small payloads: conn
  state, login step, typing). The producer
  posts one `CxxQtThread<App>::queue(|app| app.drain())` closure at a time (`AtomicBool`); `drain`
  works for at most 8 ms per run, coalesces repeated invalidations, and re-schedules itself. A
  lagged/overflowed channel triggers a reload of visible models (Delta Chat pattern).
- Re-entrancy rule: mutate, then emit; QML handlers that call back use `Qt.callLater`.

### 8.6 Images, avatars, media

- C++ shim (`cpp/imageprovider.{h,cpp}`, `.qt_module("Quick")`): a `QQuickAsyncImageProvider` for
  `image://kourier/{att,thumb,blurhash}/<attachment id>?v=<rev>` and
  `image://kourier/avatar/<handle>?v=<rev>`. Each request runs on the provider's own 3-thread pool
  and calls the Rust reader (`kourier-qt/src/media.rs`) through a function pointer. Rust decrypts
  the `kourier-blob` file into memory and returns `[format, flags, codec]` + the still encoded file
  (or RGBA pixels for a blurhash or a tile); C++ decodes at `requestedSize` (cover, no upscaling,
  EXIF orientation), crops avatars to a circle and zeroes the bytes. Nothing plaintext is written.
- Decoder allow-list: Qt never picks an image decoder by content, because the installed image
  plugins include EPS (runs Ghostscript), PDF, SVG, camera raw and PSD. `Codec::sniff` (Rust) tells
  JPEG, PNG, GIF, WebP, BMP, HEIF, AVIF or JXL from the first bytes, never from the sender's MIME
  type or file name, and `QImageReader` gets exactly that format with
  `setAutoDetectImageFormat(false)`. Any other content shows the blurhash or a grey tile
  (attachments) or initials (avatars). The composer's previews of the user's own files go
  through the same sniff (first 256 bytes, read off the Qt thread): a file that is not one of
  these formats by content gets an icon, whatever its name says. The files of a folder the
  user adds are sniffed the same way while the folder is listed, on a worker thread; a folder
  itself and its archive get an icon.
- Which file a timeline picture is drawn from (P1, 2026-10-13; Android's rule, its
  `Attachment.displayUri` is the file when it is here): `thumb/<id>` is the item's picture and
  the provider receives the item's PHYSICAL size (QML sets `sourceSize` in logical pixels,
  `Math.ceil(width)`, and Qt multiplies it by the `devicePixelRatio` itself: a `picture_ui` test
  logs the requests and compares them with the item). The core's `MediaEntry::timeline_blob(want)` serves the 400 px thumbnail
  (`kourier_media::THUMBNAIL_EDGE`) only when it covers `want` (the picture drawn to cover it,
  as `PreserveAspectCrop` does: `max(want.w / w, want.h / h) <= min(1, 400 / longest edge)`,
  compared in integers), or while there is no file, or when nothing is known or asked (no
  dimensions, no size), or when the file is no larger than the thumbnail. Otherwise it serves
  the FILE (but the provider, which has the bytes, takes the thumbnail instead when the file
  is not a JPEG BY ITS CONTENT, `Codec::sniff`, never the sender's MIME type, and has more than
  4096 × 4096 pixels, `MediaEntry::exceeds_timeline_pixels`: Qt decodes those whole), which the provider decodes down to the asked size and never up (a JPEG by libjpeg's
  scaling). So a thumbnail is never stretched: tall pictures and phone screenshots needed the
  file at scale 1 already, landscape pictures from scale 1.5, album tiles mostly keep the
  thumbnail. `att/<id>` (the viewer) is the file as before. The rule sits in the core type
  because `kourier-qt` does not depend on `kourier-media`; the viewmodel's URL revision says
  whether a thumbnail exists, so a picture drawn from the file before its thumbnail was
  derived is asked for again when it appears (`Image.retainWhileLoading` keeps the old pixels
  on screen meanwhile: the block delegates of a row live on while its content changes).
  Cost: a worker decrypts and decodes a file of up to a few MiB for each picture the thumbnail
  cannot cover, and Qt's pixmap cache holds the asked size (a 576 × 768 picture is 1.8 MB
  instead of 0.5 MB); there is no decode cache of our own.
- Media handle table (core facade, `MediaHandles`): when the core answers a row query it
  registers, per attachment in the rows, `attachment id → (path, key, mime)` in an in-memory LRU
  (4,096 entries, keys `Zeroizing`). The provider resolves handles without touching `core.db`, so
  image loads never queue behind timeline queries on the two UI read connections. A miss (evicted
  handle) costs one lookup on the dedicated third read connection (`resolve_blocking` on the
  provider's worker thread). Avatar handles live in `kourier-app` (`media::AvatarHandles`); QML
  sees a handle and a revision, never a path or key. A message hidden on an unconfirmed clock
  (§9.6) has no handle: hiding drops the handles of its attachments, a miss does not resolve
  them, and `OpenMedia` and `DownloadAttachment` for them are refused as for a message that is
  gone.
- GIFs, animated stickers, video, voice notes: `UiCommand::OpenMedia` decrypts to a file under
  `$XDG_RUNTIME_DIR/kourier/media/` and QML gets a `file://` URL (`AnimatedImage` and Qt
  Multimedia cannot use image providers); the file goes with the last `CloseMedia`. The core
  answers `OpenMedia` on one ordered queue; `MediaOpened` and a failed `OpenMedia`
  (`ActionFailed.subject = Attachment(id)`) both name their attachment, and the UI matches
  answers to requests by it. `AnimatedImage` detects formats by content with every
  plugin, so it only gets copies that are GIF or WebP by content (sniffed off the Qt thread); image
  blocks keep their still as poster and fallback. Video, including Signal's MP4 "GIFs", opens in
  another application (no in-app player yet).
- Opening a file in another application asks first unless its MIME type is one of the picture,
  sound and video types the core names the copy after (`kourier-media`'s table); SVG, `image/x-*`
  and unknown types keep the sender's file-name extension and therefore ask.
- Blurhash placeholders while thumbnails decode.

### 8.7 Notifications

- Policy (core): notify only for incoming, unread, non-history, non-silent messages; not when the
  window is focused on that conversation; never for `catch_up` messages (one summary when `Syncing` ends);
  muted (synced or local) 1:1 never; muted groups only for mentions/replies when allowed; reactions
  to own messages if enabled; content mode NameAndMessage | NameOnly | Hidden.
- Delivery (`kourier-desktop`): own zbus client for `org.freedesktop.Notifications` with hints
  `desktop-entry=app.kourier.Kourier`, `image-data` (avatar), `category=im.received`,
  `x-kde-origin-name` (account when several), inline reply (`inline-reply` action +
  `NotificationReplied`), `ActivationToken` handling. One notification per conversation
  (`replaces_id`), closed on read here or on another device (read sync), on delete and on edit.
  The first message of a conversation is shown at once; what follows within 250 ms of a shown
  notification is collected and replaces it once (`policy::Pace`, `COALESCE`).
- Click → activation token → `App.activationRequested(token, conv)` → QML
  `KWindowSystem.setCurrentXdgActivationToken(token); KWindowSystem.activateWindow(root)` → open the
  conversation. Inline reply → `UiCommand::SendText{conv}` and mark read.

### 8.8 Tray, single instance, activation, badge, autostart

- Tray: ksni pinned to a git rev with `provide_xdg_activation_token` (merged 2026-09-30, unreleased);
  switch to the release when it ships; fallback `QSystemTrayIcon` binding. Menu: Show, Quit;
  click toggles the window; unread state as attention icon.
- Close button hides to tray when a tray is available; Quit exits.
- Single instance + `sgnl://` and `signalcaptcha://` URLs: `org.freedesktop.Application`
  (`Activate`, `Open`) served by `kourier-desktop`; a second launch hands its URIs to the running
  instance (at most 16, each at most 16 KiB: a `signalcaptcha` link carries the captcha token).
- Badge: `QGuiApplication::setBadgeNumber` bound with `#[Self]`.
- Autostart: `~/.config/autostart/app.kourier.Kourier.desktop` with `Exec=kourier --background`.
- `.desktop`: `Categories=Network;InstantMessaging;Chat;Qt;`,
  `MimeType=x-scheme-handler/sgnl;x-scheme-handler/signalcaptcha;`, `Exec=kourier %U`,
  `StartupNotify=true`, `SingleMainWindow=true`, `X-GNOME-UsesNotifications=true`.

### 8.9 Settings

- App prefs (`kourier.toml`): tray, autostart, start hidden, compact layout, notification content
  mode, auto-download policy, log level, proxy (later).
- One setting is the core's and lives in `core.db`, not in `kourier.toml`, because it must
  hold with no UI and before any window exists: what happens to disappearing messages whose
  time is up while the clock is unconfirmed (§9.6; user decision, 2026-10-07). The settings
  page shows it as the group "Disappearing Messages" with three radio buttons and one
  sentence under each: "Hide now, erase once the time is confirmed" (the default), "Keep until
  the time is confirmed", "Erase by this computer's clock". A choice is applied at once
  (`UiCommand::SetExpiryPolicy`); there is no event for a change, so the viewmodel shows the
  choice once its command has left for the core and reads the option again behind it
  (`Query::ExpiryPolicy`), which also brings back the old value when the core could not
  store the new one. A command that could not be sent changes nothing on the page. The page
  asks for the option when it is first created (`App.settings.load()`), not at start, and
  the buttons are disabled until the core has answered.
- Account settings that the network syncs (read receipts, typing indicators, link previews) are
  shown read-only in M1 with "change on your phone"; Kourier-local ones (notifications per
  account, local mute) are editable.
- Window size via QML `Settings`; Wayland does not allow positioning.

### 8.10 The front-end contract

A front-end is one crate that draws the UI with one toolkit, plus its binary; today only
`kourier-qt` (Qt 6, QML, Kirigami, KDE Frameworks) with `kourier`. A second one (GTK 4 and
libadwaita, say) must be a new crate and binary beside it, with no change to the core, the
backends, `kourier-app`, `kourier-viewmodel` or `kourier-qt` (user requirement, 2026-10-08:
"have architecture to easy swapable with other GUI frameworks such as GTK for gnome etc, it
should not take much effort, just to be ready"). Nothing GTK is built. The audit behind this
section is `target/r1-context/boundary-audit.md` (at commit 0110a5e): of about 213k lines,
about 185k are below the seam and usable as they are; a GTK front-end with today's features
is estimated at 15 to 20k new lines (the Qt one has 27.7k with its tests), with the timeline
as the risk (GTK's list view has no bottom-to-top mode).

**May depend on:** `kourier-app` (`AppHandle`, `facade`, `desktop`, `folders`, `prefs`,
`about`, `trace`), `kourier-viewmodel`, `kourier-model` and its toolkit. Nothing depends on a
front-end except its binary.

**Gets from below:**
- rows and commands: `UiSource` (`CoreSource`, `FixtureSource`), `Changes`, the row types
  and `ListOp` diffs, all plain Rust data;
- every text built from data, translated and formatted: previews, notices, time labels,
  sizes, notification lines;
- desktop services through `kourier_app::desktop` (notifications, tray, single instance,
  autostart, preferences). A front-end does not speak D-Bus for these, and its toolkit must
  not claim the application's bus name (GTK: `ApplicationFlags::NON_UNIQUE`);
- media as handles and decrypted bytes (`AppHandle::media`, `read_avatar`, `OpenMedia`).

**Must provide:**
1. the widgets, list models over `ListOp`, and the hand-over of `UiEvent`s from the runtime
   to the UI thread, which never blocks and never touches a database;
2. a `kourier_viewmodel::i18n::Translator` over the gettext domain `kourier`, installed
   before the first row is built (KDE: KI18n; GTK: gettext);
3. a `kourier_app::desktop::Presenter`: pixels for notification avatars; the texts come
   from `kourier_viewmodel::notify`;
4. a `Calendar` and the desktop's time format, as data;
5. the About page from `kourier_app::about::AboutInfo`, and, where the desktop has a crash
   reporter, a sink for `kourier_app::process::install_panic_hook`;
6. picture decoding that keeps §8.6: bytes from the network go to the decoder
   `Codec::sniff` names, never to one the toolkit picks by content;
7. the rendering of `RichText`, icons for the icon keys in rows, colours, fonts, the look.

**Rules** (`cargo xtask deps-check` is to enforce R1 and R3; `i18n check` R4):
- R1 Toolkit and desktop-framework code (cxx-qt, Qt, KDE Frameworks, gtk-rs, libadwaita) is
  linked by front-end crates only: as a normal, dev or build dependency, directly or
  through another crate. `deps-check` holds the list of front-ends.
- R2 No toolkit type below the seam. `kourier-viewmodel` and `kourier-app` build and pass
  their tests on a machine without Qt.
- R3 Logic every front-end would need goes below the seam. A Rust file of a front-end that
  uses nothing of its toolkit is listed in `deps-check` with a reason, or it moves.
- R4 Text made in Rust goes through `kourier_viewmodel::i18n`: KDE context markers, `%1`
  placeholders, no KUIT tags (`xi18n*`), no Transcript, so plain gettext reads the same
  catalog. Each front-end extracts its own UI files into the one `po/kourier.pot`.
- R5 What the desktop decides (12/24 h, colour scheme, accent, icon theme, fonts) is read
  by the front-end and reaches the viewmodel as data or not at all. No colours in rows.
- R6 The application's id, name, version, licence and links are defined once
  (`kourier_desktop::APP_ID`, `kourier_app::about`); front-ends and packaging read them.
- R7 Shared preferences are in `kourier.toml` or `core.db`; a front-end's own (window
  size) are kept by its toolkit.
- R8 Each front-end has its own test rig (UI tests, pages, screenshots, perf). The fixture
  data and the dev-control wire format are shared.

**Known debts,** allowed until a second front-end starts (each is a move, not a redesign):
`RichText.html` is Qt's HTML subset with UTF-16 spoiler spans; media references are
`image://kourier/…` strings; icon keys are Breeze names; the picture reader with
`Codec::sniff`, `MediaRequests`, `PastedOut` and the composer's session state live in
`kourier-qt`; launch parsing lives in the `kourier` binary.

**Status, 2026-10-08:** the seam holds, and `cargo xtask deps-check` enforces R1 (with
build dependencies and through other crates; `kourier-coexist` is the one named exception)
and R3 (`xtask/src/seam.rs`: today's listed files are `pump.rs`, `shared.rs`, `media.rs`
and `dev/protocol.rs` of `kourier-qt`). KDE Frameworks are linked by `kourier-qt` only.
Below the seam: the installable `Translator` (§9.5), `kourier_app::about` (`AboutInfo`,
`about::info()`: names, the workspace version, the summary, the SPDX licence, a copyright
line, no links and no authors yet), `kourier_app::process::install_panic_hook(sink)`, and
one definition of the application id (`kourier_desktop::APP_ID`, re-exported by
`kourier-app` and `kourier-qt`; `xtask/src/install.rs` keeps a copy and a test that
compares it). In the front-end: the KI18n translator, `KAboutData` and the About page from
`AboutInfo`, the KCrash sink (§14.2 item 2, as built).

---

## 9. Cross-cutting concerns

### 9.1 Logging and PII redaction

- `tracing` + `tracing-subscriber` (`KOURIER_LOG` env filter, default `info`), `tracing-log` bridge
  for crates using `log` (libsignal, whatsapp-rust); output to stderr and a rotating file in
  `$XDG_STATE_HOME/kourier/logs/` (7 files × 10 MiB).
- Never logged: message bodies, captions, file names, names, phone numbers, usernames, profile or
  identity keys, passwords, tokens, URLs with credentials, QR contents.
- Types: `kourier_model::log::Redacted<T>` (prints `[redacted]`), service ids printed as
  `aci:…1f2e` (last 4 hex), conversation keys hashed (`conv#a1b2`). `BackendError.detail` must be
  built from log-safe values (libsignal's `LogSafeDisplay` already follows this).
- A test greps captured logs from the mock-server suite for the fixture contacts' names, numbers and
  message texts and fails if any appear.
- A net under the call sites: every formatted line passes `kourier_app::logging::redact` on its
  way to stderr and to the file. A whole UUID keeps its last four digits (`…1f2e`), a phone
  number becomes `+[number]`, a `sgnl://` or `signalcaptcha://` link keeps its scheme. libsignal's
  protocol crate prints full `<service id>.<device>` addresses on some WARN and ERROR paths, and
  at INFO when `KOURIER_LOG` lifts its cap.
- The target `kourier_signal::requests` (DEBUG, off by default) logs one line per request:
  `via`, `method`, the log-safe `route`, `status` or the transport error, `ms`. The smoke run
  turns it on (`docs/smoke-run.md`).

### 9.2 Configuration and paths

§5.1. Network environment (production | staging | local test with port/CA/trust root/zk params) is
part of `BackendConfig` and can only be changed by flags/env in debug builds (`KOURIER_SIGNAL_ENV`).
A linked account belongs to the server that linked it (`account.env` in `backend.db`, §6.5 step
6). A start for another server would get 401 or 403 for the device's credentials, which reads
as "unlinked" and ends in the wipe of §4.3 step 5; so the Signal actor compares the two when it
starts and exits `Failed(Unsupported)` on a mismatch, and the supervisor halts such an account at
once with the backend's words instead of restarting it. Nothing is deleted. A profile also has
one process: `App::start` takes an exclusive `flock` on `data/lock` for a normal launch
(`kourier_app::paths::ProfileLock`), which holds where no bus name does (offscreen, no session
bus, `KOURIER_DESKTOP=0`).

### 9.3 Error reporting

- No telemetry. A panic hook writes a redacted report (version, thread, backtrace, last 200 log
  lines) to the state dir; the next start shows "Kourier crashed last time" with "Copy report".
- Backend errors reach the UI as `ConnState`, `Failed{reason}` on messages, or `Notice` banners with
  `user_message`.

### 9.4 Security

- umask 077 at start; data dirs 0700, files 0600; tmpfs media files deleted promptly.
- A folder given to the composer (§8.3) is read with three rules. Names that start with a
  dot are left out: what the file manager hides, the user did not see when picking the
  folder (`.git`, `.env`). Symbolic links are never followed and never read: a link can lead
  to private files outside the folder, and a link to a parent would make the walk endless.
  And the packing reads no file by its path: `write_archive` opens the folder once and each
  file below that descriptor with `openat`, one name at a time, with `O_NOFOLLOW` at every
  step, so a file or a subfolder that another program swaps for a link after the walk is
  left out and counted as missing. "Add the files inside" still hands paths to the core,
  which opens them when the send is made, like any picked file. The archive is a second
  plaintext copy of files that already lie on a disk in the clear; it is private (0700 and
  0600), in the cache directory, and short-lived (§5.8). No path or file name of a folder is
  logged, and `FolderError` carries none.
- Keys in `Zeroizing`; SQLCipher raw keys; no key material in config files, logs or crash reports.
- TLS: libsignal-net pins Signal roots for chat; `Https` trusts only Signal's CA; future backends
  bring their own pins (Threema SPKI pins, `TD/apps/desktop/config/vite.config.ts:359-415`).
- Incoming content is untrusted: strict proto size limits, attachment size limits, image decoding in
  Qt off the GUI thread and only with a decoder Rust chose from the content (eight raster formats;
  pictures and avatars never go through Qt's format detection, and `AnimatedImage` only gets
  copies that are GIF or WebP by content, §8.6), files opened in another application only after a
  confirmation unless their type is a well-known picture, sound or video type (the confirmation
  escapes the sender's file name, because Kirigami's label guesses its text format), message
  RichText generated in Rust from a tag and scheme whitelist with
  all text escaped (never markup from the network, §8.4), link previews off (not in M1); the
  `sgnl://` scheme handler only focuses the window in M1 (group invite and `signal.me` links are
  later features and must never act without a confirmation); a `signalcaptcha://` link answers
  the challenge of whichever account waits for one and does nothing else. URIs from other
  processes (command line, D-Bus) are bounded: at most 16, each at most 16 KiB
  (`kourier_desktop::instance`). A web link dropped on the composer is not fetched (that would
  contact a third party from the user's address without asking).
- Text from outside in QML labels: `Kirigami.InlineMessage` and tooltips guess their text format
  (AutoText), so markup in a contact name, an account name or a backend's error text would be
  rendered and an `<img>` fetched. Rule for new QML: text from a backend, a contact or the
  account goes through `Style.plain(text)` (angle brackets become ‹ ›) where the label guesses
  its format, and every other label showing such text sets `textFormat: Text.PlainText`.
- A URL from a backend reaches `Qt.openUrlExternally` only as `https://` without white space or
  control characters (the challenge address, `conversations::browser_url` in the viewmodel);
  anything else leaves the dialog's button disabled. The browser is opened only by a click.
- Release builds set `PR_SET_DUMPABLE 0` first thing in `main` (`kourier_app::process::deny_dumps`,
  since phase I): no core dumps with keys, and other processes of the same user can neither
  `ptrace` the process nor read its `/proc/<pid>/mem`, `maps` or `smaps`. Debug builds keep
  dumps, unless `KOURIER_DEV_NO_DUMP=1` asks a debug build to do the same (the smoke run sets
  it: a debug build that holds a real account's keys must not leave them in a core dump;
  `crates/kourier/tests/hardening.rs` makes such a process abort and finds no dump). The cargo
  feature `dumpable` (used only by `cargo xtask perf`) skips it so memory can
  be measured from `/proc/<pid>/smaps_rollup` (§1.3); the feature `perf` implies it and adds
  the reference profile and a scripted session. Such a measuring build prints a note at start,
  must never hold a real account, and `cargo xtask install` refuses it. A non-dumpable process
  also hides `/proc/<pid>/root` from its own user, which `xdg-desktop-portal` uses to identify
  a caller: file dialog, drag and drop and paste are not yet checked on a real session.
- Safety numbers: identity-key changes show a system message; sends to that contact pause until
  the user confirms when libsignal reports an untrusted identity (Desktop's `OutgoingIdentityKeyError`
  flow, `SD/ts/jobs/conversationJobQueue.preload.ts:1076-1160`) [I: exact trigger rules to be
  ported in S8].

### 9.5 i18n

One gettext domain, `kourier` (`kourier_viewmodel::i18n::DOMAIN`), and one catalog
`po/kourier.pot`, shared by the QML strings, the strings built in Rust, the desktop file,
the metainfo and every front-end. Strings built in Rust go through
`kourier_viewmodel::i18n`: `trc`, `trnc` (plural) and `trfc` (arguments), each with a KDE
context marker as its first argument (`trc_noop` and `trm` for tables of messages that are
translated where they are shown). There is no context-less form for a literal. The text comes from a
`Translator` (`Send + Sync`, no toolkit type: `translate(context, msgid)` and
`translate_plural(context, singular, plural, count)`) that the front-end installs once with
`i18n::install`, before the first row is built; the first install wins and a second is
refused. With none installed the functions return the English source text, which is what
unit tests and fixture runs see. A translator only maps a message to its translation: the
placeholders are filled in by `i18n` itself after it returned, so every translator behaves
the same and a translation may reorder them. Placeholders are `%1`..`%9` (in a plural `%1`
is the count); braces are plain text. Unit tests use `i18n::with_translator`, a
thread-scoped translator; the process-wide `install` is tested in a child process.

The KDE translator (`kourier_qt::l10n::KdeTranslator`) asks `ki18nd`/`ki18ndc` with
`relaxSubs()` and no argument, so placeholders come back as they are; a plural
(`ki18ndp`/`ki18ndcp`) gets the count as its one argument, because KI18n needs it to choose
the form and writes it into `%1` itself. QML reads the same catalog through `L10n.qml` (a
`KI18nContext`) and through the engine's context object
(`KLocalization::setupLocalizedContext`), which makes QML's own `i18n*()` functions work in
every file. gettext translates nothing while the locale is `C`, `POSIX` or `C.UTF-8`. A
GTK front-end would install about 30 lines over gettext on the same domain.

The catalog is made by `cargo xtask i18n` (`xtask/src/i18n.rs`; it needs `xgettext`,
`msgcat` and `msgfmt` only, no Qt):

- `extract` writes `po/kourier.pot`. QML is read by `xgettext -L JavaScript` with the
  `i18n*` keywords; Rust by a small reader of xtask's own (gettext before 0.24 has no Rust
  language), which knows comments, strings with every escape, raw strings, char literals
  and lifetimes, takes `trc(`, `trfc(`, `trnc(` and `trc_noop(` with literal arguments and
  fails on a call whose context or message is not a literal, so nothing can stay out of the
  catalog; `// TRANSLATORS:` comments above a call become the translator's remark. The
  desktop file goes through `xgettext -L Desktop`, the metainfo through gettext's ITS
  rules; these few have no context, by gettext's convention, and the one context-less
  lookup in the code, `trc("", variable)`, is for them (the About page's summary). Every
  QML and Rust message is flagged `kde-format`. The file is the same for the same sources:
  sorted by file, sources named without line numbers, one line per string, a header of our
  own without a date.
- `check` (a lint step of both CI tiers, and a unit test of xtask) fails when the committed
  template differs from what `extract` writes now, on a message without a context or with
  one that does not start with `@`, on KUIT tags, on `{…}` placeholders, and for each
  language of `po/LINGUAS` on a missing file, a failing `msgfmt -c` or a translation whose
  `%1`..`%9` differ from its source's.
- `build [--out dir]` compiles `po/<lang>.po` into `<dir>/<lang>/LC_MESSAGES/kourier.mo`
  (default `target/locale`) and makes the pseudo-language `x-test`, every message wrapped
  in `xx…xx`. A debug build reads `KOURIER_LOCALE_DIR`, so a run that is not installed can
  show a catalog. `cargo xtask install` puts the languages of `LINGUAS` under
  `<prefix>/share/locale`; `uninstall` removes them.
- `extract` also merges the new template into every `po/<lang>.po` of `LINGUAS`
  (`msgmerge --previous`): a new message arrives untranslated, a changed one fuzzy, and
  the application shows English for both; `check` prints how many messages each language
  translates (an untranslated message is no failure).

`tests/kde.rs` proves the chain on the real application: it builds `x-test` from the
committed template and expects `xx…xx` from a viewmodel string, a plural, a string with an
argument, `L10n.i18nc`/`i18ncp`, the engine's own `i18nc` and the About summary. KI18n
keeps one directory per domain: a second `addDomainLocaleDir` replaces the first, and a
catalog looked up before the directory was set stays missing. `TRANSLATING.md` is the
translator's guide. The first language is Ukrainian (`po/uk.po`, a draft of 2026-10-08
that takes Signal's own Ukrainian terms for messenger things; the user reviews it; its
glossary and open choices are in the report the draft came with). `cargo xtask shots
--matrix --variant uk,x-test` renders the matrix pages in Ukrainian and in the
pseudo-language. Two rules for source strings came out of the first translation: a notice
about the user's own action is a message of its own ("You added %1" beside "%1 added
%2"), because in many languages the verb follows the person and "You" cannot fill a name's
slot; and a text that differs for one and for several without showing the number is two
messages chosen by `n == 1`, not a plural, because the plural form for 1 also serves 21 in
the Slavic languages. Not translated: the date composition in `time.rs` (word order, 24 h
clock; ICU4X is the research item) and the decimals of file sizes. Open: merging the
translated desktop-file and metainfo strings back at install (`msgfmt --desktop` /
`--xml`), a QML check that no `text:` holds a bare literal, the translation platform; the
lengths of time are nominative pieces ("1 година"), so a sentence cannot put them after a
preposition that wants another case.

### 9.6 Time

A `Clock` trait in core and backends (tests use a fake clock); all timers (outbox backoff, typing,
disappearing sweeper, key maintenance) take it.

A wrong or changed clock must not stall or destroy anything: a stored outbox wait longer than the
24 h budget is read as due, and the dispatcher clears its waits when it sees the clock go back by
more than 10 s; restart-failure times that lie in the future do not count. The Signal backend
compares the server time of every connect with its own clock and stays disconnected while
they differ too much (§6.4).

**Disappearing messages on a clock nobody confirmed** (`kourier-core/src/expiry.rs`; user
decision, 2026-10-07). "Its time has come" is read from this computer's clock, and a delete
cannot be taken back. A backend knows better, because its server states the time when it takes
the connection. An account's clock counts as confirmed while its backend is, or last was,
`Syncing` or `Online`. It is unconfirmed in two cases:

- from the moment its backend reports `FixClock` until a server takes its connection again.
  This is the `settings` row `clock_wrong` of the account and holds across a restart;
- from the start of the core until its backend gave its first verdict in this run (`Syncing`,
  `Online` or `FixClock`), for an account that is linked and has a backend to start: a clock
  that went wrong while Kourier was off has no stored trace, and a start without a network
  gets no verdict at all. This set is in memory. An account leaves it when no backend runs
  for it any more (logged out, halted, removed); an account whose network is not in the
  build never enters it.

What happens to a message of an unconfirmed account whose time has come by the local clock is
the user's choice, `ExpiryPolicy`. It is one setting for the whole core, stored as the
`settings` row scope `core`, key `expiry_policy`, and applied per account, because each
account has its own backend to confirm the clock:

| Option (stored name) | A due message of an unconfirmed account | Once the clock is confirmed |
|---|---|---|
| `HideUntilConfirmed` (`hide`), the default; also what no row or an unknown value reads as | hidden at once (`messages.expiry_hidden`), nothing of it deleted | erased if it is due by the confirmed time; shown again exactly as it was (read state, counters) if it is not |
| `KeepUntilConfirmed` (`keep`), what phase H built | stays readable, nothing is hidden or erased | erased if it is due |
| `EraseByLocalClock` (`erase`), Signal Desktop's behaviour | erased at once, whatever a backend says; nothing is ever held | nothing left to do |

Hidden means absent from every read that shows message content: the timeline (also around a
given message), the list preview and `last_activity`, the unread and mention counts, quotes
(a quote of a hidden message reads like a quote of an erased one: state `Missing`, no target,
no thumbnail, the sender's snapshot text stays), `Query::Revisions`, `Query::Attachment`, the
media handle table and plaintext playback copies. `MarkRead` skips hidden rows. Every command
that names a hidden message or one of its attachments (react, edit, delete, reply, retry,
cancel, download, open, delete attachment) is refused with `Refusal::Gone`. Hiding and
showing emit `TimelineInvalidated` and `ConversationsChanged`, the signals the desktop
service already closes a notification on. The UI has no rule of its own for hidden
messages. Ingest still sees the row: a late copy is a duplicate, and a reaction that arrives
meanwhile shows with the message when it shows again.

The sweeper is the only writer of `expiry_hidden`. Each look does three things in one
transaction: it erases what is due in the accounts that are not held (at most 200 messages
per transaction, files unlinked after the commit), hides what is due in the unconfirmed
accounts under the first option, and shows again what is hidden but no longer should be (not
due after the clock was set right, or the option changed). The same hide and show step runs

- at a start, in `Core::start`, before a handle is returned: the option is read from
  `core.db` there, with no UI involved, so the first query of a start already follows it;
- in the transaction that stores a change of the option (`UiCommand::SetExpiryPolicy`). The
  command is awaited in the core task, so a query sent right behind it follows the new
  option. A change to `erase` hides, in that transaction, every message that is due and
  still shown, in all accounts, and wakes the sweeper, which erases them in its batches.

The option in memory changes only on the writer thread, behind the commit that stored it,
and the sweeper reads the option and the unconfirmed accounts inside its own write on that
thread. A store that fails therefore changes nothing: no message is hidden, shown or erased
by an option that is not stored, and the UI gets `ActionFailed`. A core that is shutting
down keeps both sets as they are and the sweeper does nothing more, so an account whose
clock nobody confirmed in this run is not swept because its backend was told to stop.

The sweeper sleeps at most 60 s (`MAX_SLEEP`) while any timer is pending or anything is
hidden. That is also how it notices a clock that was set back without any backend verdict:
hidden messages show again within a minute, not at once.

What the option does not cover: an account without a running backend is swept by the local
clock under every option (to decide, §13 phase J open list); an account whose backend
reported `FixClock` and was then logged out stays unconfirmed until a relink; and a clock that
goes wrong after a backend confirmed it in the same run (a resume from suspend with a wrong
clock and no network) is not noticed.

---

## 10. Testing strategy

### 10.1 Layers

| Layer | What | Runs with |
|---|---|---|
| Unit | model (UTF-16 map, id grammars), core (ingest rules, outbox state machine, sort keys), db (migrations, wrong key), blob (round trip, tamper, truncation), viewmodel (rows, grouping, diffs), signal (provisioning cipher vectors, attachment crypto vectors from Desktop tests, padding, mapping) | `cargo test` |
| Property | ingest idempotency and permutation, `merge_conversation` at any point, monotonic state merges, text range conversion, kill at every crash point (§10.4) | `proptest` |
| Backend contract | the same scripted scenarios run against `FakeNetwork` and (where possible) the Signal backend on the mock server; conformance rules (§10.2) | `cargo test` |
| Signal transport | ConnectionManager, reconnect, ack timing, 1,000 queued envelopes with a slow stage 1 (no drop, no stall) against `FakeChatRemote` (`libsignal-net` `test-util`) | `cargo test -p kourier-signal` |
| Signal integration | full flows against `@signalapp/mock-server` 27.0.0 | `cargo test -p kourier-signal --test mock_server` (needs Node 24 + harness deps) |
| Real-server smoke | link → receive → reply (+ read sync, + group send in later runs) with a dedicated test account, only with the user's OK in that session | `cargo run -p kourier-signal --example skeleton -- --env prod` (S2, S8, S11) |
| Qt models | `QAbstractItemModelTester` via a small C++ shim, offscreen | `QT_QPA_PLATFORM=offscreen cargo test -p kourier-qt` |
| QML | QuickTest bin (`harness = false`) with the fixture backend | same |
| Screenshots | `kourier --fixture --page <name> --screenshot=<png>` (S0 has `--screenshot=<png>`, which grabs the page stack and quits) for each page, wide and narrow, light and dark | `cargo xtask shots` |
| Manual | Plasma Wayland checklist (§10.6), real-server dogfood with a dedicated test account | by hand |

### 10.2 `FakeNetwork`

Implements `Network` in `kourier-fake`: scripted login flows (rotating QR, relink with the same or
another identity, failure), conversations, incoming messages, duplicates, out-of-order modifiers
(receipt before message, edit before message), identity merges with both conversations populated,
slow acks, disconnects, transmit failures by kind including `Partial`, crash points. It uses a real
`backend.db` with `core_outbox` and the `kourier-db` forwarder, so the contract rule is exercised.
The conformance suite rejects: durable events sent with `ack: None`, group messages without
`Origin::Remote{author}`, own messages with an author, `HistoryBatch` over 500 messages; it also
runs the per-network scripts of §7.3. The UI fixture mode (`--fixture`) runs the real core on a
temp `KOURIER_HOME` with this backend.

### 10.3 Signal mock server harness

- `kourier/tests/mock-harness/` (outside the Desktop checkout): `package.json` pinning
  `@signalapp/mock-server@27.0.0` (npm, AGPL, prebuilt; pulls `@signalapp/libsignal-client` 0.100.0
  with linux-x64 binaries), committed `package-lock.json`, `harness.mjs`.
- `harness.mjs` starts `new Server({cdn3Path})` on port 0, applies Desktop's mock remote config
  (copied from `SD/ts/test-mock/bootstrap.node.ts:186-226`; 1:1 and sync sends on WS, upload form and
  multi-recipient on gRPC), writes `{port, caPem, trustRoot, zkPublicParams}` as the first JSON line
  on stdout, then serves JSON-lines commands on stdin: `createPrimary{contacts, groups}`,
  `waitForProvision`, `completeProvision{url}`, `sendText`, `sendTexts{n}` (bulk, for catch-up),
  `sendReceipt`, `sendEdit`, `sendReaction`, `createGroup`, `waitForMessage`, `waitForReceipt`,
  `waitForSync`, `setStorageState`, `rateLimit`, `unlink`.
- Node: 24.21.0 is installed with nvm; the nvm default alias stays 22, so the test runs
  `$KOURIER_NODE`, else `~/.nvm/versions/node/v24.21.0/bin/node`, else `node` on PATH if its
  major version is ≥ 24.
- The Rust test (`crates/kourier-signal/tests/mock_server.rs`) spawns `node harness.mjs`, builds the
  local test env (HTTP/2, CA DER from the PEM, trust root and zk params from the JSON), runs the
  backend actor against a temp `backend.db`, and drives both sides. The test skips with a message if
  no Node ≥ 24 is found or `node_modules` is missing.
- Known gaps (R:signal-desktop-receive §7.6): no CDSI/SVR, no `SERVER_DELIVERY_RECEIPT` envelopes,
  no real push delays; those are checked on the real server with a dedicated test account only.

### 10.4 Crash-safety tests

- Kill points (`KOURIER_FAILPOINT=<name>[:<n>]`, abort at the n-th hit): `after_inbox_commit`,
  `before_ack`, `after_backend_effects_commit` (stage-2 or job zone committed, events still in
  `core_outbox`), `after_core_commit` (before the forwarder's DELETE), `after_encrypt_commit`,
  `after_modifier_encrypt_commit` (the same point for edits, deletes for everyone and reactions),
  `after_server_accept` (before `send_progress`), `after_pending_command`.
- Property test (FakeNetwork in S3, Signal on the mock from S7): run a scripted stream that
  includes group changes with system messages, an identity change, a Storage Service merge, a
  partial group send and read syncs; kill at every point and every hit count in turn, restart,
  finish the stream, and require a `core.db` state identical to the uncrashed run (system
  messages included), each message exactly once, and no duplicate delivery seen by the mock.

### 10.5 CI commands (local; no remote CI)

`cargo xtask ci` runs, in order:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask deps-check
cargo test --workspace --exclude kourier-qt
QT_QPA_PLATFORM=offscreen cargo test -p kourier-qt
/usr/lib/qt6/bin/qmllint -I target/cxxqt/qml_modules crates/kourier-qt/qml/**/*.qml
(cd tests/mock-harness && PATH=$HOME/.nvm/versions/node/v24.21.0/bin:$PATH npm ci) \
  && cargo test -p kourier-signal --test mock_server   # Node 24, §10.3
cargo build --release -p kourier && cargo xtask symbols-check   # no SSL_/EVP_/CRYPTO_ in nm -D
QT_QPA_PLATFORM=offscreen QT_QUICK_BACKEND=software QT_QPA_PLATFORMTHEME=kde \
  QT_QUICK_CONTROLS_STYLE=org.kde.desktop cargo xtask shots   # kourier --fixture --page <p> --screenshot=<png>
```

As built, `ci` runs the fmt, clippy, deps-check, the two test runs, qmllint and the screenshots.
The two test runs go through cargo-nextest when it is installed (T1, 2026-10-13: every test in a
process of its own, several at a time across all test binaries, the suites that start the mock
harness, use the real Secret Service or open a Qt window held back by the test groups of
`.config/nextest.toml`; a `cargo test --workspace --doc` step follows because nextest does not run
doc tests), and through `cargo test` with a warning when it is not. The mock suites are part of
the workspace run (they skip without the harness unless `KOURIER_REQUIRE_MOCK_HARNESS=1`); the
harness's `npm ci`, the release build and `cargo xtask symbols-check` are run by hand. Measured on
the 6-core machine (2026-10-13, same tree, one core taken by another process): the test steps took
15.0 minutes through `cargo test` (non-Qt 669 s, Qt 233 s) and 6.4 minutes through nextest
(non-Qt 271 s, Qt 87 s, doc tests 12 s; with the harness-based suites 2, 4 and 6 at a time the
non-Qt run took 654, 360 and 271-283 s, the Qt run 103 s at 3 UI tests at a time). Not part of `ci` either:

- `cargo xtask perf [--out <file>] [--quick] [--report-only] [--idle <s>] [--runs <n>] [--no-mock]
  [--no-build] [--keep-profile]`: the §1.3 table (§13 phase I). A full run takes 13 to 15 min.
  Exit status 0 when every budget is met (or `--report-only`), 3 when a budget is missed, 1 when
  a measurement itself failed (also with `--report-only`).
- `cargo xtask install (--prefix <dir> | --user) [--no-build] [--scheme-handlers |
  --no-scheme-handlers] [--update-caches]` and `cargo xtask uninstall (--prefix <dir> | --user)`:
  the release binary, desktop entry, icon and metainfo in the XDG layout below a prefix
  (§13 phase I). Tests and agents install only into a temporary prefix.
- `cargo xtask demo` (§13 phase D onwards).

Optional later: `cargo deny check licenses bans` (needs `cargo install cargo-deny`), a weekly job that
compiles the WhatsApp adapter against whatsapp-rust HEAD.

### 10.6 Manual Wayland checklist (per release)

Native wayland (`QT_QPA_PLATFORM` unset; About page shows platform `wayland`; `xlsclients` does not
list Kourier); taskbar icon and grouping
by `app_id`; notification click raises and focuses the window; tray click raises; fractional
scaling 125 %/150 %; IME (fcitx5 or ibus) in the composer; drag a file from Dolphin; paste an image;
file dialog is KDE's; suspend/resume reconnects within 10 s; network switch reconnects.

---

## 11. M1 implementation plan

Each step is a vertical slice that ends with `cargo xtask ci` green. Order follows real
dependencies, and the riskiest choice (own layer on libsignal-net) runs end to end in S2, before
the core schema is frozen in S3. S2 and S6 onwards need the mock harness (Node 24.21.0 installed;
`npm ci` of the mock server is open decision 4). Real-server runs (S2, S8, S11, S16) need a
dedicated test account and the user's explicit OK in that session (open decision 3); without it
they are skipped, not replaced.

**S0. Workspace skeleton and build plumbing** (committed, `51ee9db`)
- Scope: workspace and crates from §2.2 as stubs, `.cargo/config.toml`, `rust-toolchain.toml`,
  `xtask` (`ci`, `deps-check`), `kourier` bin showing an empty Kirigami window through `kourier-qt`
  with the `App` singleton, `kourier/CLAUDE.md`.
- Done [V]: `cargo xtask ci` (fmt, clippy `-D warnings`, deps-check with a self-tested whitelist,
  tests, qmllint, headless `--screenshot=<png>`) passes; links with rust-lld; empty-shell baseline
  26.9 MB PSS (§1.3); `nm -D` shows no `SSL_`/`EVP_`/`CRYPTO_` symbols.

**S1. Storage foundation, secrets, process coexistence**
- Scope: `kourier-db` (SQLCipher custom-crypto build, raw key, pragmas incl. `secure_delete`,
  migrator, synchronous `zone`, generic `core_outbox` + forwarder), `kourier-blob`, master key via
  oo7 with passphrase fallback, `key.wrap` + account secrets (§5.2), XDG paths, `KOURIER_HOME`,
  `cargo xtask symbols-check`.
- Done when: a wrong key fails cleanly and the file has no `SQLite format 3` header; deleting an
  account secret makes its `key.wrap` unopenable; one binary runs a libsignal-net TLS handshake, a
  SQLCipher query, `QSslSocket::supportsSsl()` and a QtNetwork HTTPS request in one process, and
  its `nm -D` is clean (PSS recorded); migration, blob tamper/truncation, key-wrap, Secret Service
  (skipped without a session bus) and forwarder-crash tests pass.

**S2. Signal walking skeleton: link, receive one, reply one**
- Scope: `SignalTransport` trait + `LibsignalTransport` (auth, unauth and provisioning sockets;
  local test env and `env::PROD`), `tests/mock-harness`, provisioning cipher,
  `PUT /v1/devices/link`, key generation and upload, store traits (identity, session, prekeys,
  Kyber triples) on a first `backend.db`, zones, stage 1 with ack-after-commit, minimal stage 2
  (text `DataMessage` → `Message` into `core_outbox`), the forwarder into a test sink that commits
  to a scratch table and completes the `Ack`, unsealed 1:1 send (prekey fetch,
  `process_prekey_bundle`, `message_encrypt`); example `kourier-signal --example skeleton`. It
  uses a first cut of the contract types it needs (`Envelope`, `Ack`, `IncomingMessage`), which S3
  completes. The code stays; the real core replaces the sink in S6.
- Done when: against the mock, linking completes, `phone.sendText` lands in the sink exactly once
  (the ack leaves only after the stage-1 commit; a kill before it redelivers), and
  `phone.waitForMessage` gets Kourier's reply; with the user's OK the same three steps pass against
  production with the test account. Provisioning vector, QR URL and store conformance tests
  (identity change archives sibling sessions; Kyber triple reuse rejected).

**S3. Contract, core ingest, FakeNetwork**
- Scope: `kourier-model` (structured `NetMsgId`, `Origin`, `MsgTarget`, §7.3 items),
  `kourier-backend-api`, `core.db` schema, registry, supervisor and lifecycle (add, unlink wipe,
  relink, remove, rebuild from `key.wrap`), ingest with pending modifiers, `merge_conversation`,
  `send_state`, `pending_commands`, writer scheduler, query facade, `UiEvent`s, `kourier-fake`
  with `core_outbox` and the conformance suite.
- Done when: scripts with duplicates, permutations and merges end in the same state (proptest);
  a receipt before its message is applied on arrival; killing the fake at every point gives the
  uncrashed state; a deleted `core.db` is rebuilt with the account intact; a panicking backend
  restarts with backoff (fake clock); the per-network scripts pass.

**S4. UI shell over the fake backend**
- Scope: `kourier-viewmodel`, `ConversationListModel`, `TimelineModel` with query generations,
  `LinkAccountPage` (generic, relink), `ChatPage` with blocks, TextBlock (TextEdit + whitelisted
  RichText + spoiler overlay), media handle table + image provider shim, composer, accounts drawer,
  event pump, fixture mode.
- Done when: linking a fake account through rotating QR codes works; opening a conversation shows
  50 rows ≤ 100 ms; scrolling up pages without jumps and no frame > 16 ms with TextEdit delegates;
  text is selectable and formatted; a merge switches the open chat; narrow and wide layouts;
  viewmodel tests (shuffled query completions, RichText escaping), `QAbstractItemModelTester`,
  QuickTest for the composer; screenshots sent for review.

**S5. Outbox and send semantics (fake)**
- Scope: enqueue with allocated ids, per-conversation FIFO, backoff and budget, `Partial` with
  `only`/`is_recipient_update`, permanent failures, identity and challenge pauses, restart resume,
  retry/delete of failed messages, durable commands.
- Done when (fake clock): 3 transient failures then success keep the id; a group with one
  rate-limited member is resent only to that member; a killed process resends with unchanged ids
  and to no recipient twice; a read sync survives a kill.

**S6. Signal: account integration and complete linking**
- Scope: the skeleton becomes `SignalNetwork` wired to the core; full ConnectionManager (remote
  config, unauth rotation), `backend.db` schema v1, sync requests to own devices,
  `initial_sync = LinkOnly`, relink identity check, restart with stored credentials.
- Done when: against the mock, `provision.complete()` and `phone.waitForSync(kourier)` resolve;
  the account survives a restart; relinking a different mock account is refused before
  `PUT /v1/devices/link`.

**S7. Signal: receive pipeline**
- Scope: all envelope types, PNI rules, sealed sender validation, identity-change facts in stage 1,
  lanes, batching, unbounded listener channel with stage-1 backpressure, catch-up boundary, poison
  rules, text dispatch, failpoints.
- Done when: sealed and unsealed `phone.sendText` appear exactly once; killing at every failpoint
  (incl. `after_backend_effects_commit`) gives the uncrashed state; 500 messages queued offline
  give `catch_up` rows and one summary event; 5,000 queued messages ingest at ≥ 500/s; a
  PNI-addressed PreKey message works; FakeChatRemote with 1,000 queued envelopes and a slow stage 1
  neither drops nor stalls.

**S8. Signal: 1:1 send, sync transcripts, MSL, receipts, read sync, typing**
- Scope: content builder, sender certificates, sealed sender, session usability flags, 409/410
  repair, PNI signature, `send_progress`, sync transcripts, MSL, delivery receipts (sealed only),
  read receipts and read syncs as durable jobs, typing both ways, `SyncMessage.sent` shown as own
  messages, `NotAccepted` for non-whitelisted chats.
- Done when: `phone.waitForMessage` gets Kourier's text; phone receipts move it to Delivered/Read;
  a read on Kourier reaches the phone, also across a kill; a device added to the contact triggers
  a 409 path that still delivers; padding and content unit tests; real-server smoke run (send,
  receive, read sync) with OK.

**S9. Signal: contacts, Storage Service read, profiles, avatars**
- Scope: receive-side CDN download and `AttachmentPointer` decryption (contact-sync blob, avatars),
  contact sync, Storage Service merge through `core_outbox`, profiles, avatars into the blob store,
  account settings, pinned/archived/muted/marked-unread shown read-only,
  `initial_sync = ContactsAndStorage`.
- Done when: contacts set in the mock primary (names, profile keys, blocked, pinned) appear with
  names and avatars; `FetchLatest{STORAGE_MANIFEST}` resyncs; a 404 manifest creates nothing; a kill
  mid-merge loses no record; storage item and attachment crypto vectors pass.

**S10. Signal: groups v2 receive**
- Scope: group credentials, logs/state fetch and decrypt applied in one zone with the system
  messages, endorsements, sender-key receive (SKDM), group messages, dirty groups, group typing,
  membership drop rules.
- Done when: a mock group shows title, members and avatar; member messages arrive; a removal
  produces a system message, also across a kill after the backend commit; non-member messages are
  dropped; a failing group fetch stalls only that group's lane.

**S11. Signal: group send and retry receipts**
- Scope: per-member fan-out, then sender key + endorsements + `send_multi_recipient_message` (gRPC
  on the mock), SKDM distribution, member-device diffs, partial resend, MSL for groups,
  DecryptionErrorMessage both ways, session reset rate limit.
- Done when: every mock member gets the group text via the multi-recipient path; expired
  endorsements refresh the group first; a rate-limited member gets only the follow-up with
  `isRecipientUpdate`; a forced decryption failure on the phone leads to an MSL resend; real-server
  smoke run (group send) with OK.

**S12. Attachments end to end**
- Scope: send (import, transcode, encrypt, upload form, TUS CDN3, GCS CDN2), auto-download policy,
  thumbnails/blurhash, temp media for GIF/video/voice, limits from remote config.
- Done when: a mock image renders in the timeline (screenshot); a file from Kourier is
  byte-identical on the phone; a scan finds no plaintext attachment in `KOURIER_HOME`.

**S13. Modifiers and correctness handlers**
- Scope: edits (revisions), remote deletes, reactions, receive windows and admin deletes in the
  core, disappearing timers and the sweeper, profile-key updates, end session, view-once and
  unsupported placeholders, delete-for-me sync; UI for edited marker, tombstones, reactions, quotes.
- Done when: an edit or reaction before its target applies on arrival (both orders tested); an edit
  after 48 h is ignored; an expired message disappears on time with its files; a remote delete
  removes the notification.

**S14. Desktop integration**
- Scope: notification policy and zbus client (inline reply, activation, per-conversation replace,
  close on read sync, catch-up summary), tray, single instance, `sgnl://` open, badge, autostart,
  close-to-tray, root window release when hidden, settings pages.
- Done when: on Plasma Wayland a notification click raises the right conversation; inline reply
  sends; a second launch focuses the first instance; a long offline period yields one summary
  notification; zbus tests against a mock server on a private bus (`dbus-run-session` [V]);
  manual checklist §10.6.

**S15. Robustness**
- Scope: reconnect/backoff, `Resumed`/`NetworkChanged`, keepalive, 4409, 401/403/unlink with wipe
  and relink, clock skew, key maintenance, capabilities on start, remote config refresh, 428
  challenge with a browser hand-off (`signalcaptcha://` handler in the desktop file), PNI change
  number.
- Done when: resume after 10 min suspend reconnects within 10 s; a second socket for the device
  gets 4409 handling; mock `unlink(device)` wipes `backend.db` and shows `LoggedOut`; relinking the
  same account decrypts `phone.sendText` and keeps old history; a mock rate limit pauses the outbox
  until the challenge is answered.

**S16. Performance, packaging, dogfood**
- Scope: measure §1.3 (`cargo xtask perf`), tighten the memory budgets from the S0/S1 baselines,
  fix regressions; `packaging/` desktop file and icons; `cargo xtask install --user`; dogfood with
  the test account; the user's own account only if the user asks.
- Done when: all §1.3 budgets are met or have a plan; one week of dogfooding without lost messages
  (cross-checked against the phone).
- Status after phase I (§13): measured; every budget is met except the two memory budgets, which
  have a plan and a proposal (§1.3); the memory budgets were not tightened, the numbers say they
  must be restated; `packaging/` has the desktop file, icon and AppStream metainfo;
  `cargo xtask install` and `uninstall` exist and were run only in temporary prefixes. Open:
  the measurements that need the Plasma session or a Signal connection, `install --user` on the
  user's machine, and the dogfooding (test account and the user's OK).

---

## 12. Risks and open decisions

### 12.1 Risks

| Risk | Impact | Mitigation |
|---|---|---|
| libsignal-net is internal API and changes most releases; the bridge holds policy | compile breaks, behavior drift | one `net/` module; bump procedure §6.15; tests against FakeChatRemote and the mock |
| Server changes break linking or receiving without notice (Flare: ~8 linking hotfixes since 2022) | outage | follow Desktop's capabilities/protos per release; mock tests; small, fast bumps |
| Signal treats third-party user agents differently (REST deprecation, gRPC-only) | send/receive failures | libsignal-net already speaks gRPC; honest UA; watch Desktop's remote config keys |
| SQLCipher custom-crypto fork is a git dependency pinned to libsqlite3-sys 0.36 / rusqlite 0.38 | stale SQLite, fork upkeep | `kourier-db` isolates it; sqlite3mc in SQLCipher-v4 mode is a format-compatible fallback |
| cxx-qt 0.10 untested upstream on Qt 6.11; pre-1.0 API changes | build or runtime surprises | the spike rendered on 6.11.1 [V]; bridges kept thin; pin minor versions |
| First third-party use of libsignal-net chat; production policy (gRPC host, UA-based REST deprecation, capabilities) differs from the mock | late rework of `net/` and provisioning | walking skeleton against the real server in S2 (test account), smoke runs in S8 and S11; `SignalTransport` trait keeps an lss-style fallback cheap |
| `!Send` protocol stores and zones: a wrong design causes deadlocks (Flare 0.18.4/0.18.8) or ratchet reuse | data loss, security | synchronous zone API §6.3 (no await inside a transaction); failpoint tests |
| Durability bugs across stage 1, stage 2, forwarder and core | lost/duplicated messages or system notices | transactional-outbox rule §4.1; kill-at-every-point property tests (S3 fake, S7 Signal) |
| Storage Service read-only: roaming state drifts if the UI offered changes | user confusion | M1 hides roaming actions; local mute is clearly labeled |
| Group sends without valid endorsements | slower fan-out, rate limits (428) | endorsement refresh; fan-out fallback; partial resend only to failed members; challenge flow (S15) |
| GNOME Keyring vs KWallet: unexpected unlock prompt at login | friction | user decision; passphrase mode works everywhere |
| UTF-8 offset choice is hard to reverse | mention corruption if wrong | single conversion utility, property tests, composer prototype in S4 |
| TextEdit delegates for selectable rich text cost more than `Text` | scroll jank | measured in S4; fallback `Text` + StyledText with a select mode (§8.4) |
| Threema/WhatsApp ToS | account bans, legal | not built without the user's decision; opt-in with risk notice |

### 12.2 Open decisions for the user

1. App id and name: keep `app.kourier.Kourier` / "Kourier", or choose the final name now (changing
   it later resets notification settings and autostart entries).
2. Secret Service provider: keep GNOME Keyring (current owner of `org.freedesktop.secrets`) or switch to
   KWallet's `ksecretd` (disable `/etc/xdg/autostart/gnome-keyring-secrets.desktop`; a system change you
   make).
3. A dedicated Signal test account (separate number + phone or emulator) for the real-server runs of
   the walking skeleton (S2) and the smoke tests in S8, S11 and S16; Kourier never links to your own
   account without your explicit OK.
4. `npm ci` of `@signalapp/mock-server@27.0.0` into `kourier/tests/mock-harness/` (downloads from
   npm), needed from S2. Node 24.21.0 is installed via nvm (the default alias stays 22).
5. Later apt packages, only when the step needs them: `gettext` and `hunspell-uk` (translations,
   Ukrainian spell checking), `selenium-webdriver-at-spi` (end-to-end UI tests).
6. Threema: accept that its ToS allow third-party clients only for testing before any Threema work.
7. WhatsApp: accept the ban risk for your own account and ship it only behind an experimental flag?
8. Storage Service write-back (archive/pin/mute/block roaming; also lifts the `NotAccepted` limit
   on starting new 1:1 chats, §6.7) as the first post-M1 item: yes/no.
9. Flatpak: not in M1 (native `~/.local` install); confirm.

---

## Appendix A. Host setup

Already installed [V, `dpkg-query` 2026-10-05]: `protobuf-compiler` and `libprotobuf-dev` 3.21.12,
`qt6-declarative-dev` 6.11.1, `qt6-base-dev`, `cmake` 4.4.3, `clang`/`libclang-dev` 18, `perl`, `make`,
`pkg-config`, `libdbus-1-dev`, `ffmpeg`, Qt Multimedia QML, Kirigami 6.30, kirigami-addons 1.13.1, KDE
QML modules `org.kde.{notification,kwindowsystem,ki18n,prison,sonnet,kitemmodels,desktop}`,
`fonts-noto-color-emoji`, `lld-18` (not on PATH; `-B` fallback for rust-lld), Node 22.23.2 (nvm
default) and Node 24.21.0 (nvm, `~/.nvm/versions/node/v24.21.0/bin/node`). Rust 1.99 with clippy,
rustfmt, rust-analyzer, rust-src; rustc's self-contained rust-lld (LLD 23.1.1) is the linker.
Nothing else is needed before S2.

Later, only when the step needs it: `gettext` (translations), `hunspell-uk` (spell check),
`selenium-webdriver-at-spi` (AT-SPI UI tests).

Traps: `/opt/qt/6.9.3` first on PATH (pinned away by `QMAKE`); cxx-qt forces gold unless rustflags
contain a `-fuse-ld` flag; running cargo inside `refs/libsignal` installs toolchain 1.98.1; `npm`
on PATH is Node 22's; `git log -S` in the partial clones under `refs/` fetches blobs from the
network (use `git -c gc.auto=0`, avoid pickaxe).

## Appendix B. Source map for implementers

| Kourier area | Read first |
|---|---|
| Linking | `SD/ts/textsecure/Provisioner.preload.ts`, `ProvisioningCipher.node.ts`, `AccountManager.preload.ts:1081-1525`, `WebAPI.preload.ts:2950-3005` |
| Connection | `SD/ts/textsecure/SocketManager.preload.ts`, `WebsocketResources.preload.ts`; `LS/rust/net/src/chat.rs`, `chat/server_requests.rs`; `LS/rust/bridge/shared/types/src/net.rs`, `net/chat.rs:530-660` |
| Receive | `SD/ts/textsecure/MessageReceiver.preload.ts`, `processDataMessage.preload.ts`, `ts/messages/handleDataMessage.preload.ts`, `ts/messageModifiers/` |
| Send | `SD/ts/textsecure/SendMessage.preload.ts`, `OutgoingMessage.preload.ts`, `ts/util/sendToGroup.preload.ts`, `ts/jobs/helpers/sendNormalMessage.preload.ts`; `LS/rust/net/chat/src/api/messages.rs` |
| Retry/MSL | `SD/ts/util/handleRetry.preload.ts`, `handleMessageSend.preload.ts` |
| Groups | `SD/ts/groups.preload.ts`, `ts/services/groupCredentialFetcher.preload.ts`, `ts/util/groupSendEndorsements.preload.ts`; `LS/rust/zkgroup/src/api/groups/` |
| Storage Service | `SD/ts/services/storage.preload.ts`, `storageRecordOps.preload.ts`; `refs/libsignal-service-rs/src/storage_service.rs` |
| Attachments | `SD/ts/AttachmentCrypto.node.ts`, `ts/util/uploads/tusProtocol.node.ts`; `refs/libsignal-service-rs/src/attachment_cipher.rs` |
| Mock server | `SD/packages/mock-server/src/api/{server,primary-device}.ts`, `SD/ts/test-mock/bootstrap.node.ts` |
| UI patterns | `refs/neochat/src/app/qml/Main.qml`, `src/timeline/TimelineView.qml`, `src/rooms/RoomListPage.qml`; `refs/cxx-qt/examples/qml_features/rust/src/{custom_base_class,threading}.rs` |
| Contract prior art | mautrix-go `bridgev2/networkinterface.go`, `event/capabilities.go`; `WA/src/types/durability_hook.rs` |

## 13. As built: decisions taken during implementation

Phase A (S1, contract, mock harness, Signal foundations; 2026-10-06). These refine the sections
above; where they differ, this list wins until the sections are folded.

- §2.2: `kourier-db` uses tokio (`Notify`, `oneshot`, `time`) for the forwarder and writer helper.
  `kourier-app` returns raw `Zeroizing<[u8; 32]>` keys (it may not depend on `kourier-db`); the
  composition root wraps them. `kourier-coexist` is a test-only crate (row above).
- §3.1: Threema user ids are `threema:<ID>` (every grammar has a `<scheme>:` prefix). Log display of
  ids: phone-like schemes fully hidden, others show the last 4 characters; future `username:` /
  `email:` schemes must join the hidden list.
- §3.8: `ConversationFeatures.{formatting, files, disappearing}` are `BTreeMap`s (canonical order
  for the features hash); added `reactions: Support`, `ErrorKind::Expired`,
  `RecipientState::Sent{sealed}`, `Conversation.meta`.
- §4.2: `allocate_message_id` returns `AllocatedId {local, sent_at}`. `AccountContext::new(ContextParams)`
  returns `(AccountContext, CoreEnds)`; `CoreEnds` holds the command sender, envelope receiver, conn
  receiver and the UI/local-prefs senders; `AccountHandle {exited, thread}`; queue bounds are
  `COMMAND_QUEUE`/`EVENT_QUEUE`; helper `spawn_account_thread`. `Command::Transmit.req` and
  `Body::Sticker` are boxed (clippy `large_enum_variant`; JSON unchanged). Personal data and secrets
  in the contract are `Redacted<_>` (login inputs, QR data, pair code, challenge token, proxy URL,
  display name); serde is transparent and every model type has a log-safe `Debug`.
  `IncomingMessage.reactions: Vec<HistoryReaction>`. New ephemeral `BackendEvent::DownloadProgress
  {attachment, done, total}`; `AttachmentDone` is durable (sent with an ack). New `DurableCommand`
  (serde form of `pending_commands`), `Envelope::validate` → `ContractViolation` (§10.2 rules), and
  the `core_outbox` row codec: 2-byte LE format header (1) + JSON array of non-ephemeral events.
  `BackendNotice::{IdentityChanged{user}, ChallengeRequired{url}, Error(_)}`; `BackendConfig {env:
  Production | Staging | LocalTest{host, port, ca_cert, trust_root, zk_params}, proxy, log_filter,
  initial_sync}`; `UiPresence {focused, visible, idle}`; `BlobTarget {path, key}`. `UiEvent` lives
  in `kourier-core::facade`, not in the model.
- §5.2: `key.wrap` = `KOURWRAP`, v1, mode (1 Secret Service, 2 passphrase with inline secret),
  16-byte uuid, length-prefixed network name, 12-byte nonce, 48-byte ciphertext; KEK info ends with
  the hyphenated uuid. Under `KOURIER_HOME` the Secret Service `profile` attribute is
  `<name>@<absolute home>`, so test runs never touch the real master key. `KOURIER_HOME` layout:
  `<home>/<kourier|kourier-name>/{data,config,state,run}`. Missing `XDG_RUNTIME_DIR` is an error.
  Passphrase mode stores an HKDF check value; Argon2id 64 MiB, t=3, p=4. The Secret Service client
  opens the `default` alias and never prompts unless asked.
- §5.3/§5.6: SQLCipher 4.10 with the RustCrypto provider [V: `cipher_provider`]; `cipher_log_level
  = NONE`. Forwarder batches whatever accumulated while an envelope was in flight (≤ 100 rows), with
  no idle delay. Failpoints live in `kourier_db::failpoint` (`KOURIER_FAILPOINT=name[:n]`, abort;
  debug builds or feature `failpoints`); the forwarder hits `after_core_commit` between ack and DELETE.
- §5.8: blob format = StreamBE32, header `KOURBLOB` v1 (20 bytes) as AAD of every chunk; aes-gcm
  0.11 + aead-stream 0.6 (same aead generation as libsignal). `MediaCache` reports `is_tmpfs()`
  instead of refusing a disk directory (dev/tests); callers warn.
- §6.4: every env uses `DnsResolver::new_with_static_fallback` (the static-only resolver needs
  libsignal-net `test-util`); `DnsResolver::on_network_change` must run on `spawn_blocking` (it
  panics on a runtime thread). `finish_connect` gets an empty gRPC override map for now (libsignal
  defaults to gRPC); typed libsignal-net-chat sends, keepalive, reconnect, unauth rotation and
  provisioning rotation arrive in phase B. HTTPS responses are buffered (streaming variant needed for
  big CDN downloads; whether upload hosts need public roots is checked in S12).
  `LocalTestEnv` leaks its host strings to `'static` (libsignal-net config needs it; tests only).
- §6.5: provisioning validation is stricter than Desktop (identity public key must match its private
  key; AEP validated even with a master key; strict E.164). No Desktop provisioning vector exists; the
  tests use an independent known-answer vector and lss's device-name vector (attributed).
- §10.3: harness commands so far: `ping`, `createPrimary`, `completeProvision{primary, url}`,
  `waitForMessage{primary, from?}` (reports wire `envelopeType` and inner `messageType`; the harness
  wraps `PrimaryDevice.decrypt` to detect sealed sender), `sendText`, `waitForSync`, `unlink`,
  `setRemoteConfig`, `shutdown`. The Rust driver (`crates/kourier-signal/tests/support/harness.rs`)
  owns a temp `--state-dir`; `KOURIER_REQUIRE_MOCK_HARNESS=1` turns skips into failures,
  `KOURIER_HARNESS_VERBOSE=1` forwards harness stderr. The mock serves websockets only over HTTP/2
  extended CONNECT (HTTP/1.1 upgrade → 401); REST accepts HTTP/1.1.


Phase B (S2 walking skeleton, S3 core + fake, S4 UI groundwork; 2026-10-06):

- §6.2/§6.3 (Signal): no separate `zone.rs`; zones are `kourier_db::ZoneDb` and the stores borrow
  the zone's connection; `now_or_never` = `store::ready` (single poll, `Waker::noop`). Stage 1 uses a
  SAVEPOINT per envelope inside the batch zone; store SQL failures abort the whole batch (nothing
  acked). One `LibsignalTransport` per account actor. `SignalEnvironment::from_backend_env` derives
  storage/CDN URLs for the mock (`/storageService`, `/`, `/cdn3`).
- §6.5: login step ids `device-name, qr, linking, syncing, done, failed`; one provisioning socket at
  a time (Desktop keeps two, so a scan exactly at the 45 s rotation can be lost; revisit in S6);
  post-link steps 7-8 run in the post-connect task (crash-resumable); one-time prekeys refilled
  below 10 at every connect; sync requests best effort until the S6 job queue; no `/v2/config` yet;
  `AccountReady` is written in the zone that sets `post_link_sync='done'`, `Done` only after the core
  acked it. `ContactsAndStorage` behaves like `LinkOnly` until S9.
- §6.6: stage 2 is one serial pass (≤ 50 rows per zone); the lane key is stored but lanes do not run
  concurrently yet (S7). Decrypt failures are dropped without retry requests (S8). Identity changes
  emit only `ContactUpsert{trust: Unverified, fingerprint}`; system messages generated by a backend
  need an id scheme (decide in S13). Syncing → Online when no `core_outbox` row ≤ the QueueEmpty
  boundary is left (a DB check: rowids can restart). The forwarder is a custom loop over
  `Forwarder::{next_batch, complete}` so it can publish the acked seq.
- §6.7: text only both ways (ranges, quotes, timers, groups, attachments dropped or `Unsupported`);
  sessions must pass `NotStale | EstablishedWithPqxdh | Spqr` except right after a fresh prekey
  fetch; a send to our own ACI with no other own device is a successful no-op. Untested: the 410
  stale-device path and the 409/411 link errors (the mock does not produce them).
- §4.3/§6.4: unlink handling is minimal until S15 (`DeviceDeregistered` or 401/403 → `LoggedOut`,
  4409 → `NeedsUserAction(Reconnect)`). Mock trap: an envelope from the mock's offline queue is lost if
  the socket dies before the ack (matters for the S7 kill-at-every-point test).
- §5.4 (core): extra columns `accounts.state_reason`, `conversations.{unread_hint, read_sent_at}`,
  `messages.{out_floor, deleted_by, expiry_kind, expiry_timer_ms, expiry_started_at, silent}`,
  `reactions.removed` (tombstone), `pending_modifiers.dedup` UNIQUE; `message_revisions`
  UNIQUE(message_id, net_local) with a recomputed rank; every `account_id` cascades.
- §4.9 convergence rules: reactions last-writer-wins by `at` with tombstones (`reactions_per_user`
  applied on read); read markers from other devices are a `sent_at` watermark; counters and
  `out_state` recomputed from rows; history sort_key low bits = hash(local); delete-for-me beats
  delete-for-everyone; reactions on deleted messages dropped; receive windows checked only once the
  conversation's features are known. Permutation tests exclude arrival-dependent `sort_key`,
  `last_activity`, `read_up_to`. An envelope with history is applied as several transactions (live
  first, each `HistoryBatch` separately, ack after the last; idempotent on replay).
- §4.3 lifecycle: relink keeps the account `LoggedOut` until `AccountReady`, whose own ids must match
  the stored primary id; an exit before `AccountReady` is a failed login; startup discards `Linking`
  rows, finishes `Removing` (after one more logout attempt, unless the kept report says the device is gone) and `Unlinking` (the same rule, then `LoggedOut` with the chats kept), re-wipes `LoggedOut` credentials, rebuilds accounts from `key.wrap`
  files whose secret still opens; an unrequested `Exit::Shutdown` restarts; backoff = min·2^(n−1) +
  ≤ 25 % jitter from the account uuid, streak reset after a quiet window; missing keys → `LoggedOut`;
  a failed shred keeps the account `Removing` until next start. Key access is the
  `kourier_core::AccountVault` trait that `kourier-app` implements; `kourier_core::testing` offers
  `DirVault`, `dump`, `IngestHarness`.
- §8.5 facade: extra `UiEvent`s (AccountAdded/Removed/Error, ConversationsRemoved,
  TimelineInvalidated, ContactsChanged, Notify, CatchUpSummary, HistoryProgress, Progress, Notice,
  ActionFailed) over a broadcast channel (4096; a lagging receiver reloads); queries Accounts,
  Conversations, Timeline, Conversation, Attachment (third read connection); `UiCommand::MarkRead`
  is a durable command (pending_commands + SyncReadMarker + SendReceipts when enabled). New core
  failpoints `core_ingest_before_commit`, `core_ingest_before_ack`, `after_pending_command`.
  System messages are never unread or notified. Conversation title order: group title, nickname,
  address-book name, profile name.
- §8 (UI): rows come from a synchronous `UiSource` trait in `kourier-viewmodel` (fixture + empty
  sources; S4 swaps in the core facade). Flags `--fixture --page=<welcome|link|list|chat|settings|
  drawer> --size=WxH --screenshot=<png>`; whole-window grab via a C++ shim. Settings, link and
  add-account are stack pages (desktop `pushDialogLayer` opens a separate window and
  `pageStack.layers` triggers Kirigami anchor warnings). Pages are created eagerly (Kirigami warns
  on Component-pushed pages) — conflicts with §1.3 lazy loading; revisit with measurements in S16.
  (Phase I measured it: only the conversation list is created with the window now.)
  i18n: `L10n.i18nc` via one shared `KI18nContext` singleton (the `KI18n` singleton hangs or fails
  with KF 6.30); Rust strings via `kourier_viewmodel::i18n::tr*` (English until gettext). Voice notes
  and stickers are flags on File/Image blocks; links recolored by `App.colorLinks` (TextEdit ignores
  `palette.link`); receipt ticks drawn with QtQuick.Shapes; wide windows use a collapsed sidebar rail.
  Known KF 6.30 noise: KI18nContext prints one `installEventFilter … different thread` warning per
  process (needs the §9.1 message handler). Account nickname for badges has no model field yet.

Open for phase C: a poison policy for `core_outbox` rows the core rejects as contract violations
(today the forwarder retries any error, blocking the account); make "`AccountReady` is the first
durable event of a link" a contract rule (the core checks identity only on that envelope).

Phase C (S4 app + UI on the real core, S5 outbox, S6 Signal account integration; 2026-10-06):

- §2.2: new edge `kourier-app → kourier-db` (only `DbKey`). New test-only crate `kourier-e2e`
  (real core + real backends against local test servers; may use core, signal, fake, db,
  backend-api, model, tokio). `kourier-app` default features are `["signal", "fake"]` during M1;
  `kourier-qt` and the bin use `default-features = false` + `fake`; the bin has its own `signal`
  feature (default on). With `fake`, Welcome also offers a "Test network" demo account.
- §2.5/§8.2: the Qt message handler lives in `kourier-qt` (C++ shim with `qInstallMessageHandler`
  → tracing; KI18nContext thread warning dropped); `kourier_qt::prepare_environment()` sets the style
  env before any thread starts. No Secret Service → the app exits with a clear error (passphrase
  dialog not built yet). Logs: stderr + `logs/kourier.log` (7 × 10 MiB, 0600).
- §8.5: `UiSource` grew the whole UI data flow (events, reload, open, paging, login view, presence,
  drafts, typing, notices, tick); `send_text` returns `Result<Option<MessageId>>` because ids are
  assigned asynchronously. Timeline reloads re-read from the oldest loaded row with room for 50
  more (rows above never jump); a burst at the bottom keeps paging newer rows. `ActionFailed` is
  classified by text because the facade does not export `CoreError`. A quote without an author is
  treated as our own. Measured: first 50 rows reach the model in 2 ms (release).
- Fixture mode (`--fixture`, `--fixture-link`, pages `qr` and `history`): the real core with the
  fake network on a temporary profile (passphrase-mode keyring with cheap Argon2 settings, fixed
  "now", UTC calendar), seeded through the fake's fixture script (16 conversations × 120 messages,
  ~0.75 s). `packaging/` holds the `.desktop` file (validates) and the SVG icon.
- §4.8 (core outbox): rows are Queued, Transmitting or Failed; Failed rows stay for Retry and never
  block the queue; a row is deleted when every recipient has the message, on cancel, or on an echo.
  `send_state` for own attempts follows outbox rules (Sent is final; Retry resets non-Sent to
  Pending; `only` = request minus Sent recipients). Allocator watermark in
  `settings(scope='account:<id>', key='last_alloc_ms')`. Pauses are in memory: Challenge and
  Unauthorized/LoggedOut pause the account until the backend is Online again, a retry, or an actor
  restart; UntrustedIdentity marks the head `Failed{UntrustedIdentity, retryable}`, emits
  `Notice(IdentityChanged)` and holds the conversation until `UiCommand::ConfirmIdentity`, a retry or
  a cancel. A Partial outcome with an UntrustedIdentity member marks only that member failed. A
  dropped Transmit reply is transient; ≤ 4 transmits in flight per account; `retry_after` can
  lengthen backoff. Outgoing timers set `Expiry{AfterSend}` at enqueue. `SendText` refuses
  non-Active accounts, blocked conversations and empty text, and clears the draft. Text only so far
  (edit/delete/react sends, attachments and in-place edit of a queued message come later).
- §4.2 additions: `UiCommand::ConfirmIdentity{account, user}`, `ConversationDetail.draft`,
  `CoreError::InvalidInput`, `ContractViolation::DurableBeforeAccountReady`,
  `Envelope::validate_linking`. Poison policy: `core_outbox` migration v2 adds `core_outbox_poison`
  (in every backend.db); `OutboxSource::quarantine`; `Forwarder::{run_with, drain_with, reject}`
  quarantine rows the core rejects as non-retryable and continue (`run`/`drain` still retry all).
  Core ingest marks `CoreError::Json` non-retryable, so such rows are quarantined (kept, not lost).
- §6.5/§6.12 (Signal): `AccountReady` is written in the link transaction together with the account
  and the sync-request jobs (first durable event); `post_link_sync='done'` and `Done` follow once
  those jobs finish and the core acked. Job queue `jobs(id, kind, key, priority, cmd UNIQUE,
  attempts, next_at, expires_at, last_error, created_at, UNIQUE(kind, key))`, kinds
  `remote_config` (repeating), `capabilities`, `sync_request`; Desktop backoff; sync requests expire
  after one day; 4xx (except 408/413/425/429), protocol and decode errors drop a job. KEYS requested
  Desktop-style (≤ daily while the AEP is missing); sync requests re-sent on the first start of a new
  Kourier version, and on the start after one of the three was dropped (the drop clears
  `account.app_version`, `maintenance::job_dropped`; phase L). `/v2/config` with ETag (entries `desktop.`, `global.`, `cds.`, `client.`) →
  `AccountCaps` (max/receive attachment bytes, long text 2048); gRPC override entries stored but not
  used yet. Two provisioning sockets around the rotation (oldest closes before the next code shows;
  `SignalNetwork::with_qr_rotation` for tests). Rebuild re-sends `AccountReady` + stored caps. New
  account columns `remote_config_etag, remote_config_at, keys_requested_at, app_version`; table
  `remote_config(name, value)`. Logout still a no-op (the mock has no device delete route;
  built in phase G with a harness route).
- Open for phase D: `kourier-signal`'s own forwarding loop (`outbox.rs`) must use
  `Forwarder::reject` (today it retries a rejected row forever and drops undecodable rows);
  serialize sends to one recipient (sync-request jobs and Transmit can both touch our own-ACI
  session); `Done` can sit on "Syncing" for the jobs' one-day budget under persistent errors (UI
  hint); Rebuild should re-request contact sync once S9 exists.

Phase D (live demo, S7 receive pipeline, S8 1:1 send; 2026-10-06):

- Dev tooling (debug builds only, absent from release binaries [V: `strings`]): `--dev-control`
  JSON-lines channel on stdin/stdout (`{id, cmd, args}`; commands add_account, fill_login,
  submit_login, open, page, send, wait, screenshot{settle_ms}, state, quit; events channel,
  login_step, login_qr, account_state, error); its cxx-qt bridge is `kourier-qt/src/dev_ffi.rs`
  (cxx-qt-build accepts one bridge directory per QML module). `KOURIER_DEV_KEYRING=passphrase` +
  `KOURIER_HOME` = fixed-passphrase dev keyring (never touches the Secret Service).
  `cargo xtask demo [--out] [--interactive]` (xtask/src/demo.rs) drives the real app against the
  mock harness; light/dark come from a restart on the same profile (kdeglobals is read at startup).
  `KOURIER_SIGNAL_ENV=local:` accepts the harness's camelCase ready line.
- UI: names fall back by id scheme (aci/pni "Signal user ⋯last4", e164 the number, threema
  "Threema user", lid/wa "WhatsApp user"); nameless avatars get a person icon; the timeline follows
  the newest row unless the user scrolled away (drag, flick, wheel; not scroll-bar drag).
- §5.6/§6.6 (S7): one `core_outbox` row per inbox row; up to 8 stage-2 lanes concurrently; lane key
  = conversation (`group:` + hex SHA-256(master key)[..16] for groups until S10); network steps are a
  time-boxed (10 s) `NetworkSteps` hook (none in production yet); rows parked after a content failure
  are skipped by the catch-up boundary and backpressure count; network parks are in memory.
  Content mapped already: text, reactions, deletes, edits, receipts, typing, sync sent/read/
  configuration/messageRequestResponse; unsupported visible content → `Body::Unsupported` with the
  raw DataMessage (`BackendBlob v=1`); dropped poison rows → `Placeholder::Undecryptable{resendable:
  false}`. TimerChanged/SessionRefreshed system messages reuse the DataMessage id; IdentityKeyChanged
  still waits for the S13 id scheme. Sealed-sender numbers → `ContactUpsert{identifiers: [Phone]}`
  (no `IdentityMerged{e164→aci}` yet). Migration 0003 adds `peers(e164, profile_key,
  expire_timer_version, share_my_number, …)`. Measured catch-up ingest ≈ 2,450 msg/s (debug build).
  Harness re-sends envelopes the mock loses from its offline queue (up to 50 times).
- §6.7 (S8): text DataMessage with UTF-16 ranges, profile key, timer + version ≥ 1, protocol version
  6 with ranges, 80-byte padding; PNI signature while `share_my_number` (cleared on the contact's
  delivery receipt). Sealed sender with access keys (signal-crypto AES-CTR, checked against zkgroup's
  vector), four Desktop sealed-sender states, two cached sender certificates (refresh 1 h early),
  unauth socket rotated 5 min after opening, 401/403 → unsealed resend in the same Transmit;
  409/410 repair on both paths; 404 → unregistered; 428 → Challenge; 413/429 → RateLimited.
  `send_progress` skips accepted recipients (and the transcript) on a repeated Transmit. Sync
  transcripts unsealed, not urgent; a failed transcript becomes a one-day job (Note to Self: the
  transcript is the send). MSL rows are written after server acceptance (records the repaired device
  list); purge hourly (also done_commands, send_progress). 1:1 sends use raw REST (typed
  libsignal-net-chat sends and gRPC overrides are still open). Bodies > 2048 bytes are refused
  (long text is S12).
- §4.8/§5.5 (S8): command jobs are keyed `<cmd>:<8 bytes of SHA-256(payload)>` with `done_commands`
  (7 days) instead of uniqueness on the core command id. Receipts, read syncs and delivery receipts run
  only while Online. Jobs are at-least-once (a repeated read sync can reach the phone).
  Accepted-chat sources before S9: a phone transcript with a profile key, a profile-key-update
  transcript or a message-request Accept (Block clears); until then 1:1 chats are NotAccepted (no
  receipts/typing; sends refused with "start this chat on your phone"; the demo accepts contacts via
  the harness). The backend emits 1:1 features (formatting full, max text 2048, receive windows,
  typing/receipts on; reply/edit/delete/attachments off until their steps). Incoming typing honours
  the synced setting. New tables/columns: msl_*, send_progress, sender_certs, delivery_receipts,
  done_commands, jobs.payload, peers.features_hash, account.phone_number_sharing.
- Open for the next phase: AUTOINCREMENT on `core_outbox.seq` (kourier-db) and on core
  `pending_commands.id` (ids are reused after deletion; poison rows can overwrite, command keys had to
  move to payload hashes); retry requests both directions (DecryptionErrorMessage, MSL resend,
  NullMessage, rate limits; §6.14 items 16-17); typed libsignal-net-chat sends; IdentityKeyChanged
  system messages (S13 id scheme); Kirigami `ToolBarPageHeader.qml:61` warning on openConversation;
  quote font size; release throughput measurement.

Phase E (S9 contacts/storage/profiles, S10 groups receive + retry requests, S14 desktop, S12 core
side, id fixes; 2026-10-06):

- Ids: `core_outbox.seq` (kourier-db migration 3, table rebuild) and core `pending_commands.id`
  (core migration 2) are AUTOINCREMENT; `core_outbox_poison` has its own key. (`messages.id`, sent
  to backends as `LocalMsgId`, followed in phase F.)
- §2.2: new edges `kourier-fake → kourier-blob, kourier-media`. The media handle table lives in the
  core facade (`MediaHandles`, `CoreHandle::media()`, `resolve_blocking` for the image provider's
  worker thread). Core-owned blobs live under `accounts/<uuid>/blobs/core/…` (unreferenced-file
  cleanup only there; backends own the rest of `blobs/`). Facade additions: `UiCommand::{SendFiles,
  DownloadAttachment, OpenMedia, CloseMedia}`, `UiEvent::MediaOpened{attachment, path}`,
  `CoreConfig::media_cache` (the app sets it to `$XDG_RUNTIME_DIR/kourier/media`; the core uses
  `attachments/` below it), table `attachment_downloads`, column `attachments.derived_at`.
  `SendFiles` is refused for attachment kinds the conversation's features do not list.
- §5.8 policy: history imports, blocked chats and pending requests never auto-download; images,
  stickers, voice notes and videos capped at 25 MiB; audio/files follow `files_max_bytes`; unknown
  size counts as within limits (the backend enforces while streaming); above the receive limit →
  `Failed{retryable:false}`. Timeline rows carry attachment metadata, never keys or pointers.
  Known gap: a resumed download can run twice (harmless). (The missing `AttachmentDone` request id
  was added in phase F.)
- §6.9-6.11 (S9): streaming AES-256-CBC + HMAC attachment decryptor (digest/plaintext hash, padding,
  size) into `kourier-blob` (file appears only after verification); `DownloadAttachment.remote` =
  `BackendBlob v2` holding an encoded AttachmentPointer. Contact sync as jobs (`contact_sync`,
  `contact_sync_wait`), applied in zones of ≤ 500; a full sync clears names of unlisted contacts.
  Storage Service read: manifest version written only in the last zone (a kill re-fetches only
  missing records); raw records kept for the write path; triggered after link, at start, on
  FetchLatest and after a new storage key. Profiles: versioned unauth fetch with access key, fallback
  auth, name/about/emoji decrypted (about + emoji travel in `Contact.meta` BackendBlob v1 JSON), avatar
  from CDN0; jobs `profile_fetch`, `profile_refresh` (hourly, ≤ 50 stale). Link gate (`Done`) waits for
  the contact sync (or 60 s / 3 failed attempts) and the first storage sync. Accepted chats now include
  address-book contacts. Content from blocked senders: data messages, edits, typing dropped. A contact
  record creates a chat only when it changes roaming state or is pinned. Unset phone-number sharing
  counts as nobody. Blocked-list sync compared with its own previous list (`blocked_sync`). Rebuild
  restarts Storage Service sync and contact sync and refetches profiles. (The interim avatar files
  in `$XDG_RUNTIME_DIR/kourier/media/avatars`, `kourier-app/src/avatars.rs`, were replaced by the
  §8.6 image provider in phase F.)
- §6.8 (S10): zkgroup credentials, group state + logs (Desktop-inclusive fetch), decrypted title/
  description/timer/avatar/members/profile keys, endorsements stored raw with validated expiry,
  changes + system messages in one zone, sender-key receive, membership drop rules, group refresh via
  the job queue's backoff. System message ids: `gv2:<8-byte group id hex>:<revision>:<index>`, author =
  change source (own ACI → OwnOtherDevice), silent. No SystemEvent variants yet for invites, roles,
  access, bans, labels (state only). Group features: `cannot_send = Other("Sending to Signal groups is
  not available yet")` until S11. Embedded signed group changes are verified but used only when the
  logs are closed to us. `NetworkSteps` returns `LaneInputs{group, degraded}`; lanes cut batches at a
  group change.
- §6.6/§6.7 retry requests (S10): decrypt failures → DecryptionErrorMessage to every device of the
  sender (original timestamp), `Undecryptable{resendable}` placeholders replaced by resends; incoming
  retry requests answered from the MSL or by a NullMessage on an archived session, with rate limits.
  PLAINTEXT_CONTENT accepted only when it carries a DecryptionErrorMessage alone.
- §8.7/§8.8 (S14): own zbus notification client (D-Bus activation wait, owner watching, queued
  events, 10 s forward timeout), `Notify` coalesced 250 ms per conversation, stale notifies skipped,
  catch-up summary closed on focus, Hidden mode has no reply/avatar/origin, mentions not Critical.
  Notification avatars rendered like the in-app ones (Kirigami Add-ons NameUtils looked up at run time).
  Tray via ksni git rev 547cc42 (`provide_xdg_activation_token`); red attention dot. Single instance
  claimed inside `App::start` (`app.kourier.Kourier`; other profiles `….p<12 hex>`), a second launch
  activates the first and exits 0. `DesktopMode` (off for offscreen/minimal unless `KOURIER_DESKTOP=1`).
  Autostart entry written only when the user changes the setting. Main.qml is a QtObject shell owning
  MainWindow.qml (window released after 60 s hidden, recreated on activation);
  `setQuitOnLastWindowClosed(false)`. Badge via C++ shim. Dev-control gained `drag`, `close`, and
  `state` reports window/tray/unread. ("Compact conversation list" was saved but unused until
  phase F.)
- Open for the next phase: bounded test
  parallelism for the mock suites, the manual §10.6 checklist for the user (13 items in the S14
  report). (Closed in phase H and removed from this list: blocked senders' receipts and
  preferContactAvatars.)
  (Closed in phase F and removed from this list: round avatars and the §8.6 image provider,
  images in the timeline, Signal attachment send/receive in the UI, group send (S11), `messages.id`
  AUTOINCREMENT, the avatar-file replace window and sweep (the files are gone), blob writes off the
  network runtime (attachments; avatars are still written on the account thread).)

Phase F (S11 group send, S12 Signal attachments, UI media + composer attachments + round avatars +
compact list, S13 core side, id fixes; 2026-10-06):

- Ids and schema: `core.db` is version 4. Core migration `0003_message_ids` is a new
  `kourier_db::Step::Rebuild` (`migration_rebuild!`, `Error::ForeignKeys`: `foreign_keys` off before
  BEGIN, a non-empty `foreign_key_check` rolls the run back, enforcement restored on every path).
  It rebuilds `messages` with an AUTOINCREMENT id (rows, ids and child foreign keys kept) and
  `attachment_downloads` with an AUTOINCREMENT `id` (= `kourier_model::DownloadId`), and adds
  `outbox.undo` and the index `outbox_by_conversation(conversation_id, id)`. Core migration
  `0004_outbox_ids` rebuilds `outbox` with AUTOINCREMENT as a plain step (nothing references
  `outbox`): the dispatcher keys attempts in flight by `outbox.id`, a row can go while its transmit
  is in flight (an echo from another device, expiry, conversation removal), and with a reused id
  the late answer for the old message was recorded for the new one. It is a step of its own so
  that profiles a phase-F build left at version 3 migrate too. AUTOINCREMENT continues after the
  highest id present at migration time, so an id deleted before the migration can come back once.
  `conversations.id` stays a plain key (a reused id only delays or un-pauses a queue). Signal
  `backend.db`: `0008_group_send` (`sender_key_info(group_id, distribution_id, created_at)`,
  `sender_key_devices(group_id, recipient, device, registration_id)`, `msl_payloads.group_id`) and
  `0009_attachment_uploads` (`attachment_uploads(source, pointer, uploaded_at)`).
- §2.2/§4.2: new edge `kourier-e2e → kourier-media`; `kourier-signal` now uses `libsignal-net-chat`,
  `kourier-media` and `futures-util`, `kourier-qt` uses `zeroize`. `Command::DownloadAttachment`
  gained a required `request: DownloadId`, `AttachmentDone` a `request: Option<DownloadId>` (serde
  default, skipped when `None`): an answer that names another request is ignored, a row without
  one applies to the download in flight. Downloads left over from an earlier actor are resent only
  at the new actor's first Online (the catch-up boundary). New `OutgoingKind::name()`. Facade:
  `UiCommand::{EditMessage, DeleteMessage, React, SetConversationTimer}`; compose commands take
  effect in the order they were sent.
- §4.8 (S13 core side, `modify.rs`; step 7 is new): an own edit, delete or reaction is one
  transaction that checks the conversation's features (edit window and count, delete-for-everyone
  window, reaction support and `reactions_per_user`, admin deletes; refused when `cannot_send` is
  set or the conversation is blocked), changes the local state at once (an edit is a revision that
  targets the previous revision's id; a delete leaves a tombstone, its attachments unlinked after
  the commit; a reaction is a last-writer-wins row) and queues an outbox row of the target message
  with its own network id. `Command::Transmit.local` is the target's id, so `TransmitProgress` of a
  change shows on the target. Partial retries only the failed members; echo dedup, Retry, Cancel
  and requeue touch only `kind='Message'` rows. A change refused for good is rolled back from
  `outbox.undo` and its row deleted (not kept as Failed for Retry), with `ActionFailed`; a refused
  delete for everyone becomes a `ForMe` tombstone (content and files are gone already, as in
  Desktop). `undo` becomes `Reached` after a partial attempt that reached someone: a later
  permanent failure for the rest is neither rolled back nor reported. A delete for me is always
  local; its sync row is queued only when `features.delete_for_me` is true or the features are
  unknown, and its failure is silent. It may hide a `ForEveryone` tombstone; edits, reactions and
  deletes for everyone of a tombstone are refused. Edits are text only.
- §4.8 steps 3 and 5: deleting a queued message nobody has cancels it when it was never attempted
  or the delete is for me; a delete for everyone of a message waiting for a retry (`attempts > 0`,
  Queued) leaves a tombstone and sends the Delete; a message in flight cannot be deleted. Queued
  rows of a tombstoned message other than deletes are dropped at the next scheduler pass
  (Desktop's `sendNormalMessage` gives up on erased messages). An answer is read by what the
  attempt sent, not by the row's `kind`; message and conversation are read from the row, so a
  merge during the transmit is followed (`merge_conversation` rewrites `outbox.conversation_id`,
  `merge_rows` also `outbox.message_id`). Row gone: a send's per-recipient outcome still goes to
  `send_state` (the echo case); an edit, delete or reaction records and rolls back nothing.
- Disappearing messages (`expiry.rs`): `AfterSend` starts at `sent_at`, `AfterRead` when the message
  is read (MarkRead, read markers, own-device receipts, a merge that marks messages read; a
  message that arrives already read starts at the core's now, since read markers carry no time).
  A message sent from this device starts its timer when the first recipient has it (Sent, Partial
  with deliveries, or its echo), not at enqueue, as Desktop: one queued offline longer than its
  timer is still sent. One sweeper task sleeps on the injected `Clock` until `min(expires_at)`, at
  most 60 s at a time (tokio timers stop during a suspend), hard-deletes expired rows (≤ 200 per
  transaction) and unlinks their files after the commit. `SetConversationTimer` accepts only
  values listed in `features.disappearing` (off needs a non-empty map). Timeline query: with
  `reactions_per_user` above 1 a removal hides only itself.
- §10.2 fake: checks edit/delete/react requests like a real backend (a delete for me goes to own
  devices only). New: `TransmitRule::of_kind`, `Script.catch_up_on_resume` (a restarted actor
  reports Syncing until its backlog is acked plus a clock delay), `Script::delay_reply(conv,
  delay)` (the transmit is taken at once, its answer reaches the core after the delay on the
  account clock), `FixtureSpec.modifiers` (the UI fixture has an edited message, a tombstone and a
  reaction in a few direct chats and every group).
- §6.7 step 5 (S11, `send/group.rs`, a port of Desktop's `sendToGroupViaSenderKey`): a group send
  restarts from the top after every state change, at most 10 times. Members in `send_progress`
  count as delivered; a send with `only` or with members skipped this way is partial (their
  devices keep our chain). Endorsements (`groups/endorsements.rs`) are re-received from the stored
  raw response with zk verification, once per (revision, expiry, refreshed) within a send; outside
  now+2 h … now+2 d, or when they do not match the members, the group is refreshed first, once per
  send. Our chain (`send/sender_key.rs`) is reset after `desktop.senderKeyMaxAge` (capped at
  90 days) or when a member that had it left, keeping the distribution id (Desktop's
  `resetSenderKey`). Distribution messages go 1:1 with their own timestamp per member (Desktop:
  one for the batch), are logged in the MSL with the group id and inherit the content hint and
  urgency of the send they prepare (Desktop's `sendToGroup` passes `contentHint` on); then the
  send restarts. The trust check for the sender-key path is the backend's
  (`MemberDevices::{Usable, Untrusted}` from `is_trusted_identity`), because libsignal's
  `sealed_sender_multi_recipient_encrypt` does not check it; the outcome for an untrusted member
  is Partial with that member failed, and the core holds the chat. The multi-recipient send uses
  the typed libsignal-net-chat API on the unauthenticated socket (`ChatSocket::send_multi_recipient`;
  gRPC when the chat connection has a shared HTTP/2 connection, as on the mock, else libsignal's
  websocket REST route). Answers: accepted → `send_progress` and MSL rows in one zone; 409/410 →
  device repair and restart; 401 or `TransportError::InvalidResponse` (an answer the typed API has
  no case for) → 1:1 sends for every member; connectivity, rate limits and server errors fail the
  sender-key recipients. Recipients the server reports unregistered are marked and reported failed
  (`PeerUnregistered`; Desktop counts them as sent); members already marked are skipped silently.
- §6.7 step 5, results (S11): the 1:1 fallback also covers everyone when fewer than two members
  qualify or the endorsements are unusable; a member with sealed sender off gets the send sealed
  with their endorsement token (`SealAccess::GroupSend`). There is no access-key multi-recipient
  path (XOR of access keys), as in Desktop. Results merge per recipient: all → Sent, some →
  Partial, nobody → Failed with the most pressing error (never an empty Partial, so no
  `isRecipientUpdate` transcript precedes a first copy). The sync transcript has no destination
  and lists the members this attempt reached; a follow-up sets `isRecipientUpdate` under its own
  progress key `<local>:update:<digest>` (Desktop lists every member sent so far). A group whose
  row says we are no longer a member fails the send. Group typing uses the same path (online, not
  logged) and waits behind a running send on the per-group lock (`group:<id>` in `SendLocks`).
  Retry requests for group messages are answered from the MSL with our distribution message
  attached (members only), or with it alone when nothing was logged. Changed group features are
  re-sent at startup (`features::refresh_group_features`). The S11 rate-limit check runs through
  the 401 fan-out: neither the mock nor the real server rate-limits single recipients inside a
  multi-recipient send.
- §6.7 steps 1-2 (S12 send, `send/media.rs`, `upload.rs`): the ranges of a long text describe the
  whole text; attachments are read from the core's blob (no plaintext on disk); an image that
  does not decode keeps its bytes; images and videos lose their file names; sizes over 32 bits are
  refused. Encryption follows `encryptAttachmentV2`, checked against known answers from Node's
  OpenSSL and libsignal-client's incremental MAC. The blob is a streamed request body to the
  form's absolute URL through the same `Https` client (only the environment's CA). CDN 3 (TUS):
  after a dropped connection up to 3 × `HEAD` for `Upload-Offset` and `PATCH` from there; HTTP
  errors are final. CDN 2 (GCS resumable): only the session-opening `POST` carries the form's
  headers, at most 10 `PUT` attempts; Kourier sends `bytes start-(len-1)/len` and resumes at N+1
  where Desktop sends `start-*` and resumes at N. Pointer cache: keyed by SHA-256 of the transform
  plus the core blob's path and key (or the long text itself), so the same file attached to two
  messages uploads twice (Desktop keys by plaintext hash). `TransmitProgress`: every 250 ms while
  it changes (step `Uploading`), capped at 0.99, then 1.0 once all uploads are done (before the
  message itself is sent) with no step (`phase: None`: the line under the message falls back
  to "Sending…" until a group send reports its next step); a body without uploads reports
  nothing.
- §3.8 (S12): Signal conversations list all six attachment kinds with `max_size` from
  `global.attachments.maxBytes` (100 MiB without a config), no caption on voice notes and
  `max_text_bytes` 64 KiB; re-emitted when the config changes (phase H: `max_size` is the largest
  plaintext whose encrypted upload fits that number). The backend does not re-check sizes
  (the core does); the server's 413 maps to `TooLarge`. With neither `maxReceiveBytes` nor
  `maxBytes` in the config, the backend uses 125 MiB and sends the core no receive limit.
- §6.11/§6.13 (S12 receive, `receive/media.rs`): a pointer becomes an attachment in state Remote
  whose `remote` is the encoded pointer; one without key, digest, size or CDN key stays visible as
  `Failed{retryable: false}`. Kinds follow flags and content type; a sticker is `Body::Sticker`
  with its image. Long texts get up to 3 tries 0.5 s apart inside the step's 10 s box; CDN answers
  and verification failures are final. View-once also covers our phone's transcripts;
  `SyncMessage.viewOnceOpen` is dropped. An incomplete or over-64-KiB long-text pointer in an edit
  gives the trimmed body at once. New `TransportError::BodyTooLarge`; when a transfer broke off,
  only `TooLarge` and sink errors of the writer win over the transport error. The blob timeout is
  about 107 min for 100 MiB. Downloads decrypt and write in `spawn_blocking` behind a 16-chunk
  channel, uploads read and encrypt there behind 8 chunks of 64 KiB; avatars are still written on
  the account thread.
- §8.6/§9.4 image provider (`cpp/imageprovider.{h,cpp}`, `kourier-qt/src/media.rs`): Rust is called
  through a function pointer (like the Qt message handler; no `extern "Rust"` block). Payload:
  `[format, flags, codec]` + file, or `[2, flags, w u32 LE, h u32 LE]` + RGBA pixels; flag 1 =
  round. Fallbacks: an attachment that does not decrypt, is not downloaded or is no known picture
  format shows its blurhash or an 8×8 grey tile; an unknown attachment id and a failed avatar
  return an error (QML shows initials). The decoder allow-list closes a zero-click hole found in
  review: a default `QImageReader` picks any installed plugin by content (verified: PostScript
  bytes were rendered through `/usr/bin/gs`), and an incoming "image" the core cannot thumbnail or
  any contact's avatar reached it without a click. `AnimatedImage` had the same hole (QMovie falls
  back to content detection when the suffix decoder fails): the event pump reads the first bytes
  of every `MediaOpened` path off the Qt thread (`media::note_copy`), `App.mediaOpened(token, url,
  movie)` carries the result, and `MediaCopy.movieUrl` (all that `AnimatedImage` binds to) is
  empty unless the copy is GIF or WebP by content.
- §8.6 media blocks: `viewmodel::media::is_clip` shows Signal's GIFs (MP4 clips with the GIF flag)
  as video blocks (placeholder, play badge, label "GIF"); only `image/gif` files and WebP stickers
  animate in place. `MediaRequests` (`kourier-qt/src/app.rs`): views open copies with a token
  (`App.openMedia`/`releaseMedia`); `MediaOpened{attachment}` answers the oldest request for that
  attachment (earlier ones are reported failed), a failed `OpenMedia` the oldest request, a lagged
  event channel fails all waiting requests; every answered open gets one `CloseMedia`. This relies
  on the core's single ordered media queue (`run_ordered` in `runtime.rs`). A block's `media` flag
  (open without a question) follows `kourier-media`'s MIME table; `ImageViewer.media` defaults to
  false. The "Open this file?" dialog escapes the sender's file name and forces rich text
  (Kirigami's label guesses its format, so a name with markup could load a remote image). Copies
  handed to other applications are closed when the conversation changes. A picture's height
  follows its own width, capped by the bubble's maximum width (a wider caption crops it
  slightly). Voice notes play through Qt Multimedia (`VoicePlayer.qml`; `QtMultimedia` is loaded
  on first play, so packages must depend on `qml6-module-qtmultimedia`).
- §8.6 avatars: handles live in `kourier-app` (`media::AvatarHandles`; the core's table covers
  attachments only); cropped round in C++ (also on the software renderer); the revision goes up
  when the same file comes back with another key; the table starts over at 16,384 entries. Group
  authors use their direct chat's avatar (the facade has no per-user avatar).
- §8.3 composer and list: attach button (KDE file dialog filtered to the kinds the conversation
  takes; `QtQuick.Dialogs` is loaded on first use), drag and drop over the chat, image paste (PNG
  encoded off the Qt thread into a 0600 file under `$XDG_RUNTIME_DIR/kourier/media/paste`,
  `media::PastedFiles`; removed when it leaves the strip, 120 s after sending, at exit and at
  start). The composer text becomes the caption and Send calls `SendFiles`.
  `ConversationFeatures.files` decides: no attach button when the network takes no files, a notice
  for a refused kind. The composer judges a file's kind by its extension, the core by its content,
  so a mismatch is refused later with a notice. The "Compact conversation list" setting now works
  (one line per chat, smaller avatar, unread pill, pin icon). (Phase H: the composer no longer
  judges kinds or limits the number of files; the core marks what cannot go, and a pasted file
  stays for as long as its send is in preparation.) New QML: singleton `MediaActions`,
  `components/MediaCopy.qml`, `dialogs/ImageViewer.qml`. New viewmodel API: `UiSource::{send_files,
  download_attachment, open_media, close_media, take_media}`, `MediaAnswer`, `Changes.media`,
  `ChatHeader.attach` (`AttachRules`).
- Fixture and screenshots: the fixture gains a file, a voice note and 6 contact avatars (built-in
  PNG encoder; written into the account's blob directory after linking); pages `media`, `viewer`,
  `compose`, `compact` (fixture only; `compact` saves the setting only on the fixture profile).
  `cargo xtask shots [dir]` renders 12 pages × wide/narrow × Breeze light/dark = 48 PNGs; the
  viewer is exempt from the "light is brighter than dark by 40" rule
  (`SHOT_PAGES_SAME_IN_BOTH_THEMES`); an xtask test keeps `SHOT_PAGES` equal to
  `kourier_qt::PAGES`. The media UI tests are `kourier-qt/tests/media_ui.rs` (`harness = false`,
  real core over the fake network, offscreen).
- Dev tooling (debug builds only): dev-control `attach {path}` (absolute) adds a file through
  `ComposerController::add_file`, so the conversation's attachment kinds, the 32-file limit and
  the duplicate check apply (result `{attachments: <count>}`; errors `notFound`, `failed` with the
  composer's notice; phase H: `path` or `paths`, none of these checks, no `notFound`); `state`
  gained `media` (per media block: own, block type, download state,
  shown; no names or texts). `cargo xtask demo` also has Kourier reply in the group (it fails
  unless each member gets a sealed SenderKey message), receives a photo and sends a file back (it
  fails unless both blocks show, before and after the restart; the phone decrypts the file byte
  for byte; the check reads the timeline rows, not pixels). `--interactive` gained
  `photo <contact> [caption]` and attachment lines in `inbox`.
- §10.3 harness: new commands `rateLimit` (429 with Retry-After on 1:1 sends to one member),
  `rejectGroupSendTokens`, `answerMultiRecipientUnexpectedly`, `sendStats` (sends per destination,
  multi-recipient sends, refused tokens, unexpected answers, group fetches, uploads),
  `breakSenderKeys`, `waitForDecryptFailure`, `waitForDistribution` (`{source, sealed,
  sealedGroupId, contentHint}`), `sendAttachments` (with `forge {digest, size, cdnNumber, cdnKey}`
  per attachment: a hostile sender's pointer over an honest blob), `fetchAttachment`;
  `waitForMessage` gained a `group` filter and the fields `sealedGroupId` and
  `senderKeyDistribution`. For every suite: a primary that cannot decrypt a client envelope
  records it and answers with a retry request, like a real client; multi-recipient tokens are
  verified against the payload's recipients (the mock does not check them). The mock cannot
  exercise upload resume (its TUS server names an upload after the whole POST path) and never
  hands out CDN 2 forms.
- Open for the next phase:
  - Signal: the incremental MAC is not verified on receive; sticker send;
    avatars are written on the account thread; the pointer cache is not keyed by plaintext hash
    and keeps pointers after their message is gone; an unknown CDN number fails for good (Desktop:
    CDN 0) and recipients reported unregistered count as failed (Desktop: sent), both to decide; a
    follow-up transcript lists only the members reached in that attempt; no endorsement cache
    across sends; group send tokens are plain `Vec<u8>`, not `Zeroizing`; 1:1 sends still use raw
    REST; the view-once flow (show once, then forget) does not exist; the interactive demo's
    `photo` command and `inbox` attachment lines were never run.
  - UI: no in-app video player; voice notes go to Qt Multimedia (FFmpeg), which probes the
    container by content (needs a click; a content check needs real voice-note samples from each
    Signal client); a
    copy whose `MediaOpened` was lost to a lagged channel stays in tmpfs until exit; an avatar
    file replaced in place with the same key shows the old picture until restart; real audio
    output is untested.
  - Real server and manual: the real-server smoke run (group send, attachments, upload resume on
    TUS and CDN 2, upload hosts under Signal's CA only) needs a dedicated test account and the
    user's OK; manual Plasma checks: drag and drop from Dolphin, `Qt.openUrlExternally`, the "Open
    this file?" dialog, plus the §10.6 checklist.
  - Still open from phase E: bounded test parallelism for the mock suites (run by hand with
    `--test-threads 2`).
  - (Closed in phase I and removed from this list: downloads that resume and report progress,
    upload stall detection.)
  - (Closed in phase H and removed from this list: composer previews of the user's own files
    through a plain QML `Image` (they go through the content sniff now), what blocked senders
    send, preferContactAvatars.)
  - (Closed in phase G and removed from this list: the Signal side of S13 (edit, delete and
    reaction sends, real features, the timer-change row written by the backend, media revisions,
    IdentityKeyChanged notices, the `viewOnceOpen` sync) and the UI for it (per-row action flags,
    stand-in texts by `kind_hint`); S15 except the two pieces named in phase G's list; every
    "Core:" item: the target of an edit queued behind a refused edit, `ActionFailed` for a
    target the user deleted, the Failed row of a message deleted elsewhere, edits and reactions
    of a message nobody got, Note to Self edit limits, late copies of expired messages and the
    test of the 60 s sleep cap, expiry fields in `merge_rows`, a challenge inside a Partial, the
    test that an expired message's notification closes, failures that name their attachment,
    `SendFiles.reply_to` and the refused request handed back whole, `Preview` kinds and
    `AccountRow.avatar`, long texts joined once their file is here. Also closed: remote-config
    gRPC overrides, the "listener panicked on event" line at shutdown, the unread divider after
    an own reply, the list preview of a member removal, the contrast of sender names and of the
    trash icon. The lighter pixel column at a photo's right edge was not reproduced; bubble
    widths are whole pixels now and the provider's scale no longer comes out a pixel short. The
    build artefacts tracked by mistake were untracked in `2fb049c`, and `cargo xtask ci` and the
    demo ran on the merged phase F tree before its commits.)

Phase G (S13 Signal side, S15, message actions and account states in the UI, core contract
groundwork and follow-ups; 2026-10-06/07):

- Schema: `core.db` is version 6. `0005_timer_version_long_text` adds
  `conversations.timer_version` and `attachments.long_text`; `0006_expired_messages` adds
  `expired_messages(account_id, net_scope, conv_key, net_author, net_local, expired_at)` with an
  index by key and one by time. Both are plain steps. The wrong-clock hold is a `settings` row
  (scope `account:<id>`, key `clock_wrong`), not a column. Signal `backend.db` is version 11:
  `0010_modifiers` (`peers.expire_timer`, table `sent_quotes(local, quote, created_at)`) and
  `0011_key_maintenance`, which only marks the signed prekeys and Kyber last-resort keys stored
  so far as `confirmed` (they went out with the accepted link request).
- §4.2 contract: new `Command::SubmitChallenge { token: Option<Redacted<String>>, answer:
  Redacted<String>, reply }`, which every backend must match. `token` is the
  `ErrorKind::Challenge.token` of the attempt the core paused on (the dispatcher keeps it in
  memory; `None` when the challenge was only announced through `ConnState` or a notice),
  `answer` what the browser handed back (the solution token or the whole redirect link); `Ok`
  means the network accepted it. `Command::ChangeConversation` gained `id:
  Option<AllocatedId>`, set only for `ConversationChange::SetTimer` (an id from the core's
  own-message allocator, watermark bumped); the timer-row rule is in §4.2's notes.
  `ConversationPatch.timer_version: Option<u32>` is the network's version counter of the timer a
  patch sets (Signal `expireTimerVersion`): the core stores the highest seen and drops the timer
  of a patch with a lower one (equal applies, `None` = last patch wins); the counter stays
  backend state and the core never hands it back. `Attachment.long_text: bool` (serde default,
  omitted when false) marks the attachment that carries the whole text of a message whose body
  or caption was emitted trimmed. `QuoteSnapshot` gained `mime: Option<String>` (backends may
  set it on incoming quotes) and `thumb: Option<Box<BlobRef>>` (the quoted attachment's
  thumbnail blob; only in outgoing requests, never stored, never set by a backend).
  `OutgoingKind::Message.reply_to` is now also set for media messages. `OutgoingKind::Edit.body`
  can be `Body::Media { attachments, caption }` for an own message with attachments: every
  attachment in order with `remote`, `local` and `thumb`, `caption: None` = removed, long-text
  attachments left out; an edit never changes files. `BackendEvent::Edited` on a message with
  attachments carries the new caption and the attachments it lists are ignored. A `Message`
  with `Body::System` and an own origin gets no outgoing state. The account's own avatar is the
  `ContactUpsert` avatar of `own_ids[0]`.
- Facade (`kourier_core::facade`): `MessageRow.actions: MessageActions { reply, react, edit,
  delete_for_everyone, delete_for_me, copy, take_back, until }` and `TimelinePage.now`. Each
  flag is the answer of the check its command runs: the rules live in
  `kourier-core/src/actions.rs`; `modify.rs` and `outbox.rs` call them before they change
  anything and `query.rs` calls them on the same facts at `TimelinePage.now` (core clock).
  `until` is the instant the first of `edit` and `delete_for_everyone` turns false; the core
  sends no event for time, the UI arms one timer and re-queries. `take_back` = nobody has the
  message yet (queued or failed), so either delete simply removes it. Other changes come as
  events: a row → `MessagesChanged`; the conversation's features, blocked flag or our admin
  role → `TimelineInvalidated` (ingest emits it now); account linked or logged out →
  `AccountsChanged`. `MessageRow.reply: Option<ReplyPreview>` is the quote as the core resolved
  it: `state: QuoteState { Known, Missing, Deleted }`, jump `target`, `own`, `author_name`,
  `text`, `attachment`, `file_name` and `thumb` (a media handle registered with the page). A
  quote is resolved only inside the conversation of the quoting message; one that names a
  message of another chat is `Missing`, else a group member could make the UI show text and a
  thumbnail from another chat. `UiCommand::SendFiles` gained `reply_to`.
  `UiEvent::ActionFailed` gained `subject: ActionSubject { None, Conversation, Message,
  Attachment, Files(Arc<RefusedFiles>) }`: a refused `SendFiles` comes back whole (paths,
  caption, quote; nothing of it was queued) and a failed `OpenMedia` names its attachment. An
  `ActionFailed` for an edit or reaction whose message the user deleted meanwhile is not sent
  (nor one for a refused delete for everyone whose tombstone the user hid). `Preview` gained
  `attachment`, `attachments` (count), `system` (the `SystemEvent` of a notice; user ids, the UI
  resolves names) and `kind_hint`; new `AccountRow.avatar` and `MessageRow.author_avatar`.
  `AccountRow.conn` carries the account banners: `NeedsUserAction(SolveChallenge { url } |
  FixClock { skew_ms } | Reconnect)`, `LoggedOut { reason }` (also after a restart, when no
  actor runs), `Offline { retry_at_ms }`, `Connecting`, `Syncing`; the same values arrive as
  `UiEvent::ConnState`. New `UiCommand::SubmitChallenge { account, answer }`: an empty answer is
  refused by the core, a refusal is `ActionFailed { action: "SubmitChallenge" }` and the
  challenge stays. `UiCommand::RestartAccount` for an account whose actor runs and is linked is
  passed on as `Command::Connect` (the banner's "Reconnect" after a 4409; a backend that is
  connected or connecting ignores it); a halted account restarts as before.
- §3.5.3/§4.8 own edits (core): `EditMessage` also edits the caption of an own `Media` message.
  The attachment rows and files stay, every caption is kept in `message_revisions`, an empty
  text removes the caption (a `Text` message cannot become empty), and the edit is refused where
  the file kind takes no caption. An incoming edit of a media message replaces only the caption
  and converges in any order. An own edit's target is set when it is sent
  (`outbox::take_due` → `retarget_edit`, Desktop's `getTargetOfThisEditTimestamp`), so an edit
  typed behind one the network then refuses names what the network has. Note to Self has no
  edit window and no count, sending and receiving (`actions.edit` with `until: None`).
- §4.8 a message nobody has (send row Queued without an attempt, or Failed, and no recipient
  reached) is edited in place (message row and stored request, no revision) while it has no
  revision; with an edit already waiting, further edits are revisions held behind it. Edits and
  reactions of a message whose send failed for everyone are kept locally and their outbox rows
  are held: the scheduler skips them and they are not heads of their conversation. "Retry"
  sends the message as first written, then the held rows in order, each with a fresh 24 h
  budget. Signal Desktop refuses a reaction to an unsent own message; Kourier queues it
  (CLAUDE.md rule), so `MessageActions.react` stays true. Only "failed for everyone" is held: a
  message that reached some members sends its edits and reactions to all of them, because a
  Signal client parks a modifier for a message it does not have yet. The in-place edit is still
  bound by the chat's edit window, count and `features.edit` (dropped in phase H, where a send
  that gave up on time-outs also stopped counting as "nobody has it").
- §4.8 tombstones and checks: a tombstone deletes its outbox rows other than deletes at once
  unless one is in flight, and that one goes with its answer (also a permanent failure);
  leftovers of earlier builds go at the next actor start (`outbox::reset_transmitting`, a
  correlated EXISTS over the outbox, not a scan of the account's messages). A queued Delete
  always survives, also a restart. A reply is checked by `actions::reply`: target in this
  conversation, not deleted, not a notice or placeholder, `features.reply` usable, chat can
  send. A delete for everyone of a notice or placeholder is refused. When several checks fail,
  the feature or window check reports before the empty-text check; "only text messages can be
  edited" became "only texts and captions can be edited".
- §4.8 challenges: a `Partial` outcome with an `ErrorKind::Challenge` member pauses the account
  like a whole-send challenge. Delivered members are Sent, the row stays the head narrowed to
  the challenged and retryable members, without a backoff timer; with nobody delivered the next
  attempt is still a first send. The same holds for Edit, Delete and React rows (not rolled
  back, not reported). The pause and its token live in memory: after a core or actor restart
  the head is tried once and pauses again with a fresh token.
- Disappearing messages and merges (core): the sweeper records the key of each message it
  deletes in `expired_messages`, and ingest drops a `Message` or history row with such a key
  for 45 days (`kourier_core::EXPIRED_MESSAGE_MEMORY`, Desktop's cutoff), also after a restart;
  an identity merge rewrites `net_author` and `conv_key` of remembered keys. The 60 s sleep cap
  is a pure function, checked through `FakeClock::deadlines()`. `merge_rows` keeps the expiry of
  whichever copy had one (the earlier start and end when both do); the conversation timer in a
  merge goes by `timer_version`, so a timer switched off later does not come back from the
  older side.
- §4.3 lifecycle (core): the unaccepted-actor rule of step 6 closed a bug: a relink refused as
  another account could still become that account when the refused batch held more than
  `AccountReady` (the forwarder offered the rows again while the supervisor stopped the actor).
  An unaccepted actor still gets `StartLogin`, login input, `CancelLogin`, `Shutdown`,
  `Logout`, downloads, `ConfirmIdentity` and `SubmitChallenge` (since phase H the last three no
  longer reach an actor that is linking, §4.3 step 6). The relink identity check runs
  wherever `AccountReady` reaches the core: the running loop, the drain after the actor
  reported its exit, and shutdown. `Supervisor::stop` takes an `Ingest` mode: `Linked` applies
  what still arrives; `Linking { relinking }` (the core shuts down during a login) keeps a link
  that finishes on the way out and refuses a relink as another account; `Refused` (login
  failed, cancelled or refused, account removal) answers every envelope with a non-retryable
  refusal. During a removal the envelopes of an actor that is still linking are refused, and
  `AccountReady` never changes the state of an account that is `Removing`; a removal whose
  shred fails stays `Removing` with the ids it had and is finished at the next start. Outbox
  rows survive logout and restart: a relink to another account sends nothing, the same account
  sends them once with their original ids, and downloads are resumed at link time.
- §4.7/§9.6 (S15, core side): hints are collected per account for `HINT_SETTLE` (1 s) and never
  reach a backend while its login runs; a storm lasting longer yields one command per second.
  `Resumed` re-arms the outbox backoff, the restart sleep of a crashed backend and the sweeper.
  The clock rules are in §9.6. Only a clock a backend reported is noticed.
- §6.7 (S13 send; `send/outgoing.rs`, `send/content.rs`): `Transmit` handles every
  `OutgoingKind` in 1:1 chats and groups through the existing sealed 1:1 and sender-key paths,
  with the MSL, `send_progress`, Partial outcomes and `isRecipientUpdate` follow-ups of a
  message. Edit = `EditMessage { targetSentTimestamp, a whole DataMessage with its own
  timestamp }` with `SyncMessage.Sent.editMessage`; delete for everyone = `DataMessage.delete`
  (`adminDelete` with the author when a group admin deletes another member's message); React =
  `DataMessage.reaction { emoji, remove, targetAuthorAciBinary, targetSentTimestamp }` with the
  chat's timer and (1:1) version; delete for me = `SyncMessage.deleteForMe` to own devices only
  (outcome Sent with no recipients). A quote is sent with id, author, text, UTF-16 ranges and
  the quoted attachment, whose thumbnail is uploaded from `QuoteSnapshot.thumb`. Receivers drop
  the quote of an edit that carries none and `OutgoingKind::Edit` has no `reply_to`, so the
  backend keeps the quote of every reply the account sent in `sent_quotes`: from this device at
  send time, from other own devices out of their sync transcripts (messages and edits, 1:1 and
  group), keyed by the reply's id and by each edit's id. A media edit re-sends the pointer the
  attachment arrived with, else uploads from the blob (3-day pointer cache). An edit carries
  the chat's current timer, not the message's own (the request has no expiry).
- §6.13 timers (`timer.rs`): the chat's timer and version live in `peers`. Every 1:1
  DataMessage states its chat's timer, as a timer update or as an ordinary message, from the
  peer or in a transcript of our phone. Rules: a version below ours is ignored (Desktop applies
  the timer of a stale update, the phones do not); a version at or above ours raises ours and a
  different timer changes the chat (`ConversationUpsert { timer, timer_version }` + `System
  TimerChanged`); without a version an ordinary message changes nothing (Desktop's "zero
  version"; the phones apply it) and an `EXPIRATION_TIMER_UPDATE` changes the timer and keeps
  our version. The notice of a timer update has the update's own id; a change that an ordinary
  message brings gets the id `timer:<timestamp>` and is dated 1 ms before the message. Our own
  change (`ChangeConversation { SetTimer, id }`) is one zone: version + 1, the patch, the
  `TimerChanged` row with the core's id and a `timer_update` job. The job sends the update (not
  urgent) and its transcript, retried for a day, also across a restart; `reply` is therefore
  `Ok` before the peer has it, and every later message carries the new timer and version
  anyway. Timers from the initial contact sync are stored and reach the core with the chat's
  first features. Own transcripts always carry `Expiry.started_at` =
  min(`expirationStartTimestamp` or the sent timestamp, now). A group's timer change is refused
  ("Change this group's timer on your phone"): it needs group writes.
- §3.8 Signal features (`features.rs`; each value cites its Desktop constant, and the module
  doc says why each limit is kept): reply Full; reactions Full, one per user; edit 24 h and 10
  times, received 48 h (no limits in Note to Self); delete for everyone
  `global.normalDeleteMaxAgeInSeconds` (1 day without a config; not in Note to Self), received
  1 day longer; delete for me; disappearing `AfterRead` [30 s, 5 min, 1 h, 8 h, 1 day, 1 week,
  4 weeks] in 1:1 chats, none in groups; `admin_delete` and `AccountCaps.flags["admin_delete"]`
  from the remote-config entry `desktop.adminDelete.send.prod` (present and not `false`), which
  also gates receiving in the core. `AccountCaps` carries `delete_for_everyone_max_age_secs`,
  so a config change re-sends every chat's features. Kept limits: the receive windows (every
  official client applies them), the 24 h send window (receivers ignore an edit whose server
  time is more than 48 h after the original, and an accepted edit may wait 24 h in the outbox),
  the server's delete window, one reaction per user (the protocol's meaning), and 10 edits
  (the common send cap; whether the phones show an eleventh could not be read from Desktop's
  sources). The timer menu is Desktop's list; the backend itself takes any whole second up to
  4 weeks. (Phase H read the phones' sources: no receiver enforces 10 edits, the cap is 100 now,
  and the edit send window is capped by the server's delete window. Phase J read them for the
  timer: no receiver enforces four weeks, the bound is 2,147,483,647 s now.)
- §6.13 receive (S13): a remote edit is dated with the envelope's server timestamp, so the 48 h
  window cannot be dodged by backdating (own edits from the phone keep their timestamp). Quotes
  carry snapshot text, attachment kind, file name and MIME. `SyncMessage.deleteForMe` message
  deletes become `Deleted { for_everyone: false, by: None }` in 1:1 chats, Note to Self and
  groups. A received `adminDelete` names the member as target author and the admin as `by`.
  `PROFILE_KEY_UPDATE` stores the key and refetches the profile (nothing shown). The view-once
  row stays an `Unsupported` placeholder after `viewOnceOpen` and nothing of it is kept raw.
  IdentityKeyChanged notices are written where the core has the conversation (the 1:1 chat and
  every group the contact is in), with ids `keychange:<8 bytes of SHA-256(key), hex>[:<8 bytes
  of the group id, hex>]`: the id scheme open since phase B, the same when a change is reported
  twice. Reactions: `emoji.rs` `is_one` accepts exactly one emoji by shape (UTS #51: one emoji
  with variation selector, skin tone or tag sequence, ZWJ sequences, keycaps, a flag of two
  regional indicators; at most 64 bytes; no emoji table, so newer emoji pass); sending anything
  else is refused with `InvalidInput`, because the receivers would drop it.
- §6.4 connection (S15; `connection.rs`, `connection/{backoff,health}.rs`): the connection task
  takes hints from the actor (`Nudge::{Connect, NetworkChanged, Resumed}`). A hint that arrives
  while a connect attempt is on its way is kept: if that attempt fails without an answer from
  the server, the next one starts at once and the delays start over (one retry per burst); a
  refusal by the server (HTTP status, Retry-After) keeps its delay. A check asked for by a
  hint replaces a routine keepalive that is still unanswered (its late answer is ignored). A
  non-2xx keepalive answer or silence replaces the socket. After 4409 and `AppExpired` the task
  ends `ConnectionEnd::Held` and the actor is held; `Command::Disconnect` holds it the same way:
  hints do not reconnect, `Command::Connect` does (before, the next hint reconnected after a
  `Disconnect`). A 403 or 422 on our PNI
  prekeys also starts the unlink procedure. `ChatSocket::disconnect` waits up to 2 s for the
  listener's last event, which removed libsignal-net's "listener panicked on event" line at
  shutdown. The unauthenticated socket does not repeat a failed connect before the same backoff
  delay (requests in between fail at once as transient); hints drop the socket and the delay.
  Where Kourier differs from Desktop on purpose: the long backoff tail is used while the server
  was never reached since the last reset, and a check sent because of a hint has a 5 s timeout
  (Desktop: 30 s unless it believes it is offline).
- §6.4/§9.6 clock (S15): the server time of the connect response is the first event of a
  socket, so nothing is received on a socket whose clock check fails. While the clock is off:
  socket closed, `NeedsUserAction(FixClock { skew_ms })` (local minus server, whole seconds), no
  connect attempts; the clock is looked at every 10 s and on `NetworkChanged`/`Connect` without
  connecting. `Resumed` makes exactly one connect to hear the server's time anew, because the
  monotonic clock that carries the server time forward stands still during sleep.
- §4.3 steps 5 and 7 (S15): in the unlink procedure stage 2 settles the inbox without a socket
  (`stage2::settle`; a group message is applied with the last known group state or stays
  behind); rows that still need the network are lost with the wipe. `Command::Logout` sends
  `DELETE /v1/devices/<id>` (5 s, best effort) and then runs the same procedure with the 2 s
  shutdown drain. A relink keeps `AccountId`, conversations and messages and sends what waited.
- §6.12 keys (S15; `keys.rs`): `maintain(kind, Mode::{OneTime, Routine, Forced})` is Desktop's
  `maybeUpdateKeys`; each one-time kind is refilled on its own count; empty key lists are
  omitted from `PUT /v2/keys`; `confirmed` is set after the upload; the triples of deleted
  last-resort keys go with them. Job keys: `''` routine, `'pni'` forced after a number change
  (retried by the job queue for up to 24 h; Desktop gives up after one attempt).
- §6.6 number change (S15; `number_change.rs`): `SyncMessage.pniChangeNumber` is consumed in
  stage 1 (`Disposition::Consumed`): only from our own ACI on our ACI, with the envelope's
  `updatedPni`, registration id 1..=0x3FFF, and keys signed by the identity it brings (stricter
  than Desktop). It is applied in the envelope's savepoint: account row, PNI identity,
  confirmed signed prekey and last-resort key; old PNI prekeys, sessions and triples deleted;
  `share_my_number` marks, sender certificates and group credentials dropped; the forced PNI
  key job queued; `AccountReady` with the new own ids written in the same zone. Later envelopes
  of the batch already use the new account. The new E.164 is stored but not reported (reported
  since phase H as `AccountReady.number`).
- §4.6/§6.7 challenge (S15; `challenge.rs`): `ErrorKind::Challenge` is reported only for a 428
  whose body has a non-empty token and lists "captcha" in `options`; any other 428 is
  `RateLimited` with the response's Retry-After (the server leaves the captcha out when the
  account used up its attempts, and a pause on it could not be lifted). `SubmitChallenge`
  strips `signalcaptcha://` and a trailing `/` and sends `PUT /v1/challenge { type: "captcha",
  token, captcha }` with the newest token the actor saw (from Failed and Partial outcomes) or
  the core's: 2xx → Ok; 413/429 → `RateLimited` (60 s without a Retry-After); 428 → `Challenge`
  with the new token. Token and solution are never logged.
- §6.4 remote config (S15): the `desktop.libsignalNet.grpc.*` entries reach libsignal-net as
  gRPC overrides (`SignalTransport::remote_config_changed`, at actor start and after each
  fetched config, in force from the next socket). The switch table is a copy of nine keys from
  `LS/rust/bridge/shared/types/src/net/remote_config.rs` (v0.104.0; the bridge crate is no
  dependency): diff it at every libsignal bump (§6.15).
- §8.3 message actions (UI): `MessageRow.actions` → viewmodel `RowActions` → model role
  `actions`. The context menu (`timeline/MessageMenu.qml`: right click, long press, Menu key or
  Shift+F10) and a toolbar beside the bubble (React, Reply, More; only More where the window
  has no room) offer what the flags allow: quick reactions, Add Reaction…, Reply, Edit, Copy
  Text, Show Reactions, Send Again (`RetryMessage`), Cancel Sending or Delete (`CancelMessage`
  for a message nobody has), Delete… (`dialogs/DeleteDialog.qml`; Delete for Everyone only when
  the core offers it). One tokio wake-up re-reads the flags when the first window ends
  (`MessageActions.until`; no polling; it assumes the core clock runs at real-time speed from
  the page's `now`). Reaction chips toggle our reaction, their tooltip names who reacted, right
  click or long press opens the who-reacted list (`ReactionsPopup.qml`, anchored to the bubble;
  our own entry can be removed). The picker is six quick emoji plus the whole grid
  (`composer/EmojiGrid.qml`: static list, no search, skin tones or recents; the category tabs
  share the grid's width and the grid leaves room for its scroll bar).
- §8.3 composer modes: edit mode (banner "Edit Message" or "Edit Caption", prefilled text, Send
  = `EditMessage`, empty allowed only for captions, an unchanged text sends nothing, the draft
  typed before comes back afterwards; Up in an empty composer edits the newest editable own
  message, Esc leaves edit or reply mode) and reply mode for text and files (banner with
  author, snippet, kind icon and thumbnail). The quote block is drawn from `ReplyPreview`
  (author, text or kind and file name, thumbnail, the missing and deleted states); activating
  it scrolls to the original and flashes it, loading the window around it
  (`TimelineAnchor::Around`) when it is not loaded; "Jump to Latest" then reloads the newest
  page. A refused `SendFiles` puts files, caption and quote back.
- §3.6/§8.4 edits keep formatting (`kourier-viewmodel/src/edit.rs`, `EditForm`): the composer
  edits the body with each mention written out as "@Name". On save the changed stretch is what
  lies between the longest common beginning and end of old and new text; formatting ranges and
  mentions outside it are kept (UTF-8 offsets shifted), a formatting range that contains the
  change follows it, text typed at a range's edge without white space joins it, a range the
  change cuts into keeps its untouched part, a mention or link whose own text changed becomes
  plain text. Without this an edit made in Kourier revealed a spoiler and turned mentions into
  text for every receiver. Two separate changes in one edit count as one stretch.
- §8.3 timers, stand-ins, previews, look: the chat header has a timer action with a menu of the
  values the conversation offers (current one checked) and a timer mark; messages carry a timer
  mark; timer changes are system rows. The three disabled stub actions of the header (Search,
  Mute, Details) are gone; muted is a mark beside the subtitle. Content Kourier does not show
  gets a stand-in text and icon by `kind_hint` (`preview::unsupported_summary`,
  `unsupported_label`), also in list previews; list previews use attachment kind and count and
  the system event. The unread divider goes away after our own answer (at send, or when an own
  row from another device arrives). Own bubbles are the accent tint with normal text (§14.1);
  sender names, links, accents and secondary text go through `Style.readable()` and
  `Style.secondary()` (≥ 4.5:1, hue kept; `qml_smoke` checks 11 name colors on own bubble, peer
  bubble and page, light and dark). `TextBlock`, `QuoteBlock` and `FileBlock` take the bubble
  color as `surface` instead of an `own` flag. Tombstones use `trash-empty`: Breeze icons with
  NegativeText or NeutralText classes (`edit-delete`) ignore the color `Kirigami.Icon` asks
  for. The drawer shows the account's own avatar. This covers parts of the R1 gate early
  (§14.2 item 3 own bubbles and contrast, item 4 context menu and no disabled stubs).
- §8.3 account states (UI): `components/AccountBanner.qml` above the conversation list shows the
  most pressing state of the accounts: a quiet line for offline, connecting and syncing; "Link
  Again" (`Relink` of the same account, then the link page); "Verify…"
  (`dialogs/ChallengeDialog.qml`); the clock-skew notice; "Reconnect"; "Restart" for a halted
  account. A logged-out account's composer says why it cannot send. Relink: `CoreSource`'s
  login state records `relink: Option<AccountId>`, so "Try again" on a failed relink sends
  `Relink` for the same account and not `AddAccount`; `App::banner_activate` sets
  `LoginController.network` from the account so the link page shows that network's steps.
  Challenge: "Verify…" only emits `challengeRequested(account, url)`; the browser is opened by
  a button in the dialog; the answer comes through `App.submitChallenge` (pasted) or as a
  `signalcaptcha://` link, which `kourier-app` (`desktop::handled_uris`) accepts and
  `UiSource::challenged_account()` routes to whichever account waits for a challenge.
  Notices: every `ActionFailed` shows as the composer's notice with a text of its own, chosen
  by matching the core's error text (by error kind since phase H); the conversation list page
  shows the same notice (with
  Dismiss) whenever the composer cannot: no chat open, a chat that cannot be written to, or a
  narrow window where the list covers the chat (`ConversationListPage.showsNotice`).
- §8.3 timeline keyboard and scrolling: Tab or Shift+Tab into the timeline makes the newest
  message current (the list sees the release of that Tab); Up and Down move between messages,
  Menu or Shift+F10 opens the current row's menu, Esc returns to the composer. A pointer never
  makes a row current: a click into a message's text gives that text the focus (selection,
  Ctrl+C) and the view stays; the first arrow key afterwards starts at that row. When the focus
  leaves the timeline (not for its own popups) `currentIndex` goes back to -1, because a
  `ListView` keeps its current item in view and would move the timeline under the pointer.
  Toolbar buttons take focus by Tab only, so a click on React or Reply leaves the typing focus
  in the composer. `atTop`/`atBottom` are distances (within margin + one grid unit), not
  `atYBeginning`/`atYEnd`: a programmatic jump to the newest row rests one margin short of the
  end. "Jump to Latest" shows more than 6 grid units from the newest message or when the loaded
  window ends before it.
- Viewmodel and bridge API: `UiSource::send_files` takes `reply_to`; `MediaAnswer::Failed`
  carries `Option<AttachmentId>`; `Changes` gained `jump` and `files`;
  `UiSource::challenged_account()`; composer invokable `replyTo(conversation, message)`
  replaces `replyTo(message, author, text)`; new `App` invokables `react`, `deleteMessage`,
  `retryMessage`, `cancelMessage`, `setTimer`, `jumpTo`, `jumpToLatest`, `copyText`,
  `bannerActivate`, `submitChallenge` and `findMessage` (fixture pages and tests); new QML
  singletons `TimelineActions` and `Emoji`; `TimelineView` aliases `rowList` and `messageMenu`
  for tests; the image provider's C++ shim also sets clipboard text.
- Fixture and screenshots: pages `quotes`, `menu`, `react`, `reactions`, `edit`, `reply`,
  `delete`, `timer`, `placeholders`, `offline`, `loggedout`, `challenge`, `clock`; `PAGES` and
  `SHOT_PAGES` hold 25 pages and `cargo xtask shots` renders 100 PNGs. Fixture mode (also
  `--fixture-link`) gives the core a `FixtureClock` (`kourier-app/src/fixture_clock.rs`): real
  time, started 7 minutes after the newest fixture message. With the system clock every fixture
  message was outside the edit and delete windows and own `AfterSend` timers were swept at
  once. `kourier-app` `FixtureOptions` gained `showcase` (default true) and `state:
  FixtureState`; `FixtureInfo` names the quotes, timer and stand-ins conversations. The
  showcase adds three chats: the fake's `showcase_quotes()` ("Pim Quill": quotes of a known, a
  missing and a deleted message, a photo with an edited caption) and `showcase_timer()` ("Rue
  Lumen": timer notices and `AfterRead` messages), and from `kourier-app` a chat with one
  stand-in per kind ("Wren Tallow"), reactions of several members and the account's own avatar.
  The pages `offline`, `loggedout`, `challenge` and `clock` end the fixture in that account
  state. Two workarounds for the fake live in `kourier-app` (removed in phase H, which fixed
  the fake).
- §10.2 fake: `kourier-fake` `FixtureSpec.showcase` (default false, so its own fixture is
  unchanged) and `FixtureSpec.state: FixtureState { Online, Offline, Connecting, Syncing,
  Challenge, ClockSkew, LoggedOut }` (`Challenge` shows `fixture::CHALLENGE_URL` and any answer
  brings it Online; `LoggedOut` unlinks with the history kept). `TransmitRule::challenge(url)`
  (every send refused until `SubmitChallenge` is accepted) and `member_challenge(user, url)`
  (Partial), `Script::accept_challenge_answer`, `challenge_error`, `CHALLENGE_TOKEN` (the fake
  refuses an answer without the token it issued); own timer changes with rows and versions;
  media edits accepted; `attachments::synthetic_long_text`; `Step::Conn(any ConnState)`;
  `Step::Unlink` as the last step marks the script finished; `Script::online_while_linking()`
  and `Script::ready_with(events)` (an actor that reports Online, or sends more than
  `AccountReady`, before the core accepted it). `FakeClock::deadlines()` in `kourier-backend-api`.
- Dev tooling and demo (debug builds only): dev-control `act {on: own|other, action:
  react|reply|edit|delete, text?, emoji?, remove?, for_everyone?}` acts on the newest own or
  other message of the open chat through the same invokables as the UI (`App::react`,
  `App::delete_message`, `ComposerController::{reply_to, start_edit, send}`; `dev_act` in
  `kourier-qt/src/dev/exec.rs` must follow a rename). `cargo xtask demo` gained
  `modifiers_scene` (a contact reacts, Kourier fixes a typo, replies to her photo with a quote
  whose thumbnail is uploaded, deletes a message for everyone; it fails unless the contact's
  phone and our own phone get the edit, the quote and the delete) and, last,
  `robustness_scene` (a send challenge holds a message and shows the banner; the server lifts
  it and drops the socket, Kourier reconnects and the message goes out; the phone unlinks the
  device and the banner offers to link again). 32 screenshots; the demo ends with the account
  unlinked. New test knobs `SignalNetwork::{with_keepalive_interval, with_clock_recheck}`.
- §10.3 harness: new commands `sendData` (any DataMessage or EditMessage from parts: pairwise,
  sealed, sender key, or as the phone's transcript; targets unchecked, which is the hostile
  sender; it stamps `serverTimestamp` with the time now), `sendSync` (deleteForMe,
  viewOnceOpen, viewed, read), `waitForEdit`, `waitForMessage{kind}`,
  `sendAttachments{editTarget}`, `secondSocket`, `closeSocket`, `failAuth`, `setServerTime`,
  `failKeepalive`, `connectionStats`, `keyCounts`, `consumeKeys`, `changeNumber`, `challenge`,
  `challengeAnswers`, and the route `DELETE /v1/devices/<id>`. Changed for every suite: a
  websocket upgrade with credentials the server does not know is refused with 403 (wrong
  password 401) instead of accept-then-close; every upgrade carries `x-signal-timestamp`;
  `unlink` closes the device's socket with 4401 (`closeSocket: false` keeps the old behaviour);
  key uploads behave like the real server (`PUT /v2/keys` without `preKeys` is accepted, an
  upload without one-time keys leaves the server's alone, a second one-time upload of the
  identity that uploaded first while linking no longer fails with 400).
- §10.2-§10.4 tests: core `tests/{message_actions, account_states, followups, robustness}.rs`
  and a third mode `held` in `outbox_crash.rs` (the process is aborted at every hit of the
  three outbox failpoints around a held reaction; the recipient gets the message first and each
  exactly once); `tests/support` `start_core_on(dir, networks, vault, clock)`; `OddLogin` in
  `robustness.rs` wraps the fake and plays one login from a test thread (`AccountReady` after
  the exit report, at Shutdown, at Logout), for orders the fake's actor cannot produce. Signal:
  `tests/{modifiers, robustness}.rs` on the mock, `kourier-e2e`
  `tests/signal_{modifiers, robustness}.rs` through the real core (real 3 s timers; an edit and
  a reaction sent once across a kill at `after_modifier_encrypt_commit`), unit tests of the
  connection on libsignal-net's fake chat server with a `FakeClock`; `actor::testing::NoTransport`
  counts and can hold `connect_auth` calls; `ShiftClock` in the test core. UI:
  `kourier-viewmodel` `core_source/action_tests.rs` and `edit.rs`; `kourier-qt`
  `tests/{actions_ui, account_ui, input_ui}.rs` (offscreen, real core on the fake). `input_ui`
  drives real key and pointer events with QtTest's `TestEvent` (QML module `QtTest`, package
  `qml6-module-qttest`, created with `Qt.createQmlObject` so a missing module fails with a clear
  message). `core_ui` (fixture-link) waits for the fixture's last delivery before paging.
  `kourier-app` `desktop::e2e::notifications_end_to_end` also checks that a notification closes
  when its message expires.
- Open for the next phase (what phases H and I closed is named in the last two items;
  everything else here is still open and is carried into the newest list):
  - Core: a long text over 256 KiB or empty is not joined (the message stays trimmed);
    `AfterRead` timers started under a wrong clock keep the error, and a held account that is
    logged out stays held until it is linked and connected again; the resend of pending
    commands at link time has no test (a logout deletes them); the drain-path relink test
    relies on 300 ms of real time.
  - Contract, to decide: `OutgoingKind::Edit` has no `reply_to` and no expiry (the backend's
    `sent_quotes` covers 14 days; an edit of an older reply, possible only in Note to Self,
    loses its quote); one `admin_delete` flag serves sending and receiving and one window
    serves normal and admin deletes (Desktop has `desktop.adminDelete.receive.prod`, reads these
    entries as minimum versions, and has `global.adminDeleteMaxAgeInSeconds`);
    `QuoteSnapshot.thumb` is outgoing only, so an incoming quote of a message that is not here
    has no picture.
  - Signal: a group's timer cannot be changed (group writes); the `timer_update` job drops the
    peer send on a permanent error, an unconfirmed safety number included (Desktop retries for
    a day); contact-sync timers are taken only from the initial sync; incoming call offers are
    dropped (`SystemEvent::MissedCall` needs the phone's call events); group story replies and
    story reactions show as `story-reply` placeholders in the group's timeline (Desktop: only
    in the story viewer), to decide; stage-1 backpressure (inbox above 2,000 rows) waits
    without serving the keepalive, hints or the unlink signal, so a backlog longer than
    5 minutes ends in one needless reconnect; the unlink drain is not cancel-aware (quitting
    during it can take 10 s longer) and commands sent during it are not handled; a Retry-After
    the server gives at connect is cut short by a hint; a clock so wrong that TLS fails shows
    Offline, not `FixClock`; an unlink while offline or held is noticed only at the next
    connect, and `Logout` without a socket cannot tell the server (the device stays listed on
    the phone); not implemented: checking our PNI identity against whoami and `/v2/keys` after
    PNI decryption failures (obligation 13), a test for the group credentials refetch after a
    number change; 1:1 sends still use raw REST; harness: the mock's own senders still copy the
    client timestamp into `serverTimestamp`, the mock hands a freed device id out again on
    relink; the interactive demo has no react, edit or delete commands. Still open from
    phase F: the incremental MAC is not verified on receive, sticker send, avatars
    written on the account thread, the pointer cache (not keyed by plaintext hash, keeps
    pointers of deleted messages), an unknown CDN number and recipients reported unregistered
    (both to decide), follow-up transcripts list only the members of that attempt, no
    endorsement cache across sends, group send tokens not `Zeroizing`, the view-once flow.
  - UI: an edit with two separate changes treats them as one stretch, and the composer cannot
    create or show formatting or mentions; the emoji grid has no search, skin tones or
    recents; the jump to an original that is not loaded is tested in the viewmodel only
    (fixture chats are shorter than one page); no Orca pass. Still open from phase F: no in-app video player, voice notes
    through Qt Multimedia's content probing, a media copy left in tmpfs after a lagged channel,
    an avatar replaced in place under the same key, real audio output untested.
  - Real server and manual: the real-server smoke run (group send, attachments, upload resume
    on TUS and CDN 2, upload hosts under Signal's CA only; 401 or 403 for a removed device and
    the 4401 close, whether the server keeps one-time keys on an upload without them, the real
    428 body with `options` and Retry-After, the answer to a wrong captcha and the captcha page
    flow, a silently dead TCP connection) needs a dedicated test account and the user's OK.
    Manual Plasma checks: drag and drop from Dolphin, `Qt.openUrlExternally`, the "Open this
    file?" dialog, the challenge hand-off from the browser, the §10.6 checklist.
  - Still open from phase E: bounded test parallelism for the mock suites (run by hand with
    `--test-threads 2`).
  - (Closed in phase I and removed from this list. Core: an incoming edit without a quote
    removes the quote (`Edited.keeps_quote`); the long texts of a chat that stops being blocked
    or request-pending are fetched; `Command::Logout` follows a `CancelLogin` or a refused
    login (the window that is left is inside the Signal backend, phase I's list); `take_due`
    reads one head per conversation. Contract: a timer range beside the list
    (`disappearing_custom`). Signal: downloads that resume and report progress, upload stall
    detection. UI: `core_ui` measures Qt-thread CPU time.)
  - (Closed in phase H and removed from this list: sending many files as several messages and
    the audit of the limits copied from Desktop; the S15 leftovers (logind and NetworkManager
    listeners, `x-scheme-handler/signalcaptcha` in the desktop file, a dev-control command that
    answers a challenge). Both review bugs: a modifier sent in one chat landing on a message of
    another, and the admin delete synced from our own phone. Core: receive windows measured
    from the server time; edits of bodies that cannot be edited; edit rules on an in-place
    edit; a send that gave up on time-outs counted as "nobody has it"; `UntrustedIdentity` for
    one member inside a Partial; `DownloadAttachment`, `ConfirmIdentity` and `SubmitChallenge`
    reaching a linking actor; the challenge wait bounded by the send budget, and the core
    acting on `BackendNotice::ChallengeRequired` (a challenge announced that way still has no
    token in the contract: the backend keeps its own); the sweep at a first start under a
    wrong clock; typing to blocked or unaccepted chats; rows behind a tombstoned head (for
    removals by ingest; the sweeper's case is in phase H's list); the revision query. A reply
    to an own message nobody has stays allowed, by decision, and is tested. Fake: timer
    versions, the script replayed at relink, a relink that fails; both `kourier-app`
    workarounds are gone. Contract: events for a chat's history cleared and one attachment
    deleted (receiving), `ErrorKind::Challenge.retry_after`, `AccountReady.number`. Signal:
    the 10-edit cap (now 100), a 428 on a job, the three broken intra-doc links, smoke cases
    for `sendData`, `sendSync` and `waitForEdit`, the demo's checks of edit, quote and delete
    in Kourier's own timeline; a 428 on the multi-recipient send is mapped but libsignal-net
    does not hand it over (phase H's list). UI: stand-ins for `poll-ended` and `pin`, the
    revision history, a context menu on notices, notices chosen by error kind, the timeline as
    one tab stop, the duplicate sentence after an unlink, the reaction chips' overlap, composer
    previews through a plain `Image`. The `ChatPage.qml:16` TypeError did not show in the
    phase H demo logs. Phase E: the backend side of blocking and preferContactAvatars. `cargo
    xtask ci` and the demo ran on the merged phase G tree before its commits.)

Phase H (many files in one send and the limits audit; the two review bugs and most of the open
list of phase G: delete for me of chats and attachments, blocking, challenge retry times, the
revision history, the desktop side of S15; 2026-10-07):

- Schema: `core.db` is version 7. `0007_hidden_number` (a plain step) adds
  `conversations.hidden` and `accounts.number`. `hidden` exists so that a chat deleted on the
  phone leaves the list without the core forgetting its title, members and features: deleting
  the row would force a backend to re-emit state it cannot know was lost. Signal `backend.db`
  stays at version 11 (the avatar preference needs no column) and the fake's schema is
  unchanged (`Script.number` is read from the script at rebuild, failed relinks are counted in
  memory per process).
- §3.8 packing contract (`kourier-model/src/features.rs`): `ConversationFeatures.file_packing:
  FilePacking { albums: Vec<FileAlbum { kinds: BTreeSet<AttachmentKind>, max: u16 }>,
  reply_needs_text: bool }` (serde default, part of `stable_hash`). An album names kinds that
  may share one message and the most of them one message carries such that every client shows
  all; `max` 0 and 1 mean alone; a kind in several albums belongs to the first.
  `reply_needs_text` says a message with files cannot carry a quote (no backend sets it yet).
  Features already stored in `core.db` lack the field and decode as "every file alone", so a
  backend emits every conversation's features again after it sets its rule.
  `plan_files(kinds, caption, reply) -> FilePlan { lead_text, messages: Vec<Vec<usize>>,
  reply_unsendable }` with `message_count()`: a message holds neighbours of the selection
  (positions ascending), never files gathered from further apart; `lead_text` = the caption
  goes first as a text of its own and carries the quote, because a kind of the first file
  message takes no caption (`FileLimits.caption`) or files cannot quote; `reply_unsendable` =
  files cannot quote and there is no caption to carry the quote, and the core refuses the send.
- §4.8 step 1, §5.8 the split send (core; `outbox::enqueue_files`, `attachments/import.rs`):
  `UiCommand::SendFiles { conv, paths, caption, reply_to }` keeps its shape and takes any
  number of files of any kinds; `MAX_FILES` (32) is gone and nothing replaces it. Order of
  work: every file is inspected (kind from its first 64 bytes, size, type) before any is
  copied; if one is refused, nothing is queued and no blob stays. Then the files are copied one
  after the other in 64 KiB chunks, with at most one picture in memory (64 MiB; a larger one
  goes without thumbnail and blurhash). Then one transaction writes, per message of the plan, a
  message row with its own network id (strictly increasing; 1 ms apart on Signal) and one
  outbox row. The caption and the quote go on the first message. Where that message takes no
  caption, the caption goes first as a text that carries the quote. A caption above
  `max_text_bytes` goes first as the texts `Text::split` makes of it, the quote on the first,
  the files after it without a caption. A backend therefore sees several `Command::Transmit`
  with `OutgoingKind::Message { body: Body::Media { attachments, caption }, reply_to, expiry }`,
  each holding one album or one single file, in order through the conversation's FIFO; only the
  first carries `caption` and `reply_to`. The compose queue waits for the whole import, so a
  text typed behind a large send is still sent behind it (phase I: in the same chat; other
  chats no longer wait, and a send of more than 64 messages is stored in several
  transactions, §4.8 step 1). A kill at any of 13 hits (new
  failpoints `outbox_in_enqueue` and `outbox_after_enqueue_commit`, plus
  `outbox_after_attempt_commit`, `after_server_accept` and `outbox_before_outcome_commit`)
  leaves the send stored as none or all of its messages, each sent once to each recipient in
  order with unchanged ids, and the blobs on disk equal to the `files` rows.
  `SendText` above `max_text_bytes` is split the same way instead of failing at the backend
  (`kourier_model::Text::split(max_bytes)`: a piece ends at a line break in its second half,
  else at white space there, else between two characters; ranges are cut where a piece ends, a
  mention stays whole, a piece of white space alone is left out).
- §3.8 Signal's packing rule (`kourier-signal/src/features.rs` `file_packing`, `ALBUM_MAX`;
  the module doc cites every source): pictures and GIFs share a message as an album of at most
  31; videos, audio files, voice notes and other files travel one per message; the caption is
  the body of the first message, which also carries the quote. Why, and from where:
  - Album: all three clients draw an album as up to five tiles, the fifth with "+N", and show
    all of it in their viewer (`SD/ts/components/conversation/ImageGrid.dom.tsx`; Android
    `components/AlbumThumbnailView.java`; iOS `CVMediaAlbumView.swift` `maxItemCount`).
  - 31: receivers cut a message's attachment list at 32. Android cuts before it stores
    (`messages/SignalServiceProtoUtil.kt` `toPointersWithinLimit`,
    `RemoteConfig.maxAttachmentCount`, remote key `android.attachments.maxCount`), and there the
    long-text attachment of a caption over 2 KiB is one of the 32; Desktop cuts after it took
    the long text out (`SD/ts/textsecure/processDataMessage.preload.ts` `ATTACHMENT_MAX`).
  - Other files alone: Desktop renders only the first attachment of a message unless all are
    pictures or videos (`SD/ts/components/conversation/Message.dom.tsx` `renderAttachment`,
    `areAllAttachmentsVisual`); Android renders the first audio file, else the first document,
    else the album (`conversation/ConversationItem.java` `setMediaAttributes`,
    `mms/SlideDeck.java`) and keeps only the voice note of a message that has one.
  - Videos alone, although the clients draw videos in albums: Desktop draws an album only when
    it can draw every item, and of videos only `video/{mp4,ogg,webm}`
    (`SD/ts/util/GoogleChrome.std.ts` `isVideoTypeSupported`). One QuickTime file among
    pictures and a Desktop reader sees the album's first item and no sign that more was sent.
    The rule is per attachment kind, so it cannot let MP4 in and keep QuickTime out. The same
    gap is left for HEIC and HEIF pictures (open list). (Phase I: the rule names formats; MP4
    is in the album, HEIC, HEIF and AVIF travel alone.)
  - The Android and iOS files were read from the public Signal-Android and Signal-iOS
    repositories (main, 2026-10-07); there is no local checkout of either.
- The split on the mock, through the real core (`kourier-e2e` `tests/signal_files.rs`): one
  `SendFiles` of a GIF, a PNG, two documents, an MP3, an MP4 and a PNG with a caption reaches
  the contact's phone as six messages in the picked order (the album of GIF and PNG with the
  caption, then each other file alone), with seven uploads, every pointer valid, and our phone
  gets six transcripts; the same holds in a group, and a rate-limited send is retried without
  a second upload. Documents, audio, video and GIF files arrive byte-identical; still pictures
  arrive re-encoded (next item).
- §5.8, §6.7 step 2 send path (`kourier-signal/src/send/media.rs`): still pictures are
  re-encoded without metadata at Signal's level three (`PICTURE_LEVEL`: 4096 px, JPEG quality
  0.75, at most 3 MiB; a file up to 0.4 MiB keeps its size and kind), the "High" of the
  official composers, not Desktop's default level one; the upload cache key is `image-level-3`,
  so a pointer cached for level one is not reused. At most 10 attachments of a message keep
  their incremental MAC (`MAX_INCREMENTAL_MACS`, remote key
  `global.maxIncrementalMacsPerEnvelope`); with one video per message this can only bite on an
  edit of an album another own device sent. The GIF flag is set only on video content: it
  marks a looping video, and official clients send `image/gif` files without it.
  `FileLimits.max_size` is now the largest plaintext whose padded, encrypted upload fits
  `global.attachments.maxBytes` (`features::max_plaintext`: 102,224,512 bytes for 100 MiB), so
  the core refuses an oversized file before it is copied instead of the upload failing.
- §8.3, §8.4 composer files (UI): the picker, a drop and a paste (files copied in a file
  manager, or an image) add any number of files of any kinds in one call; adding does no I/O on
  the Qt thread (sizes and the content sniff run on `spawn_blocking`; 300 files take about
  10 ms). The strip is a list view over `AttachmentListModel`
  (`kourier-qt/src/models/attachments.rs`)
  fed by `kourier_viewmodel::FileStrip`: thumbnail or icon, name, size, remove per row, "Clear
  All", and a summary such as "46 files, 19 messages · 1.0 MB". The split comes from
  `Query::FilePlan`, one query in flight, the newest selection wins and a stale answer is
  dropped. Each row shows the number of the message it travels in; a file the send would
  refuse is marked with its reason and stays behind while the others are sent. If a send goes
  out before the plan arrived and the core names refused files (`RefusedFiles.refused`), the
  viewmodel sends the same command again without those positions and hands only the refused
  files back; refused sends are a queue (`take_refused_files` is called until `None`), and
  `refused_notice(refused, sent, here)` has its own sentences for a chat that is no longer
  open. The attach dialog starts on a "Media files" filter where a chat takes only some kinds
  and always offers "All files (*)"; the filter is a convenience, the core reads kinds from
  content.
- §8.4 files in preparation (`UiSource::preparing_files`; phase I replaced what follows by the
  send's request id, `FilesStored` and `Progress::Import`): the core has no event for "this
  `SendFiles` was stored", so the composer's "Preparing N files to send…" is a hint built from
  timeline rows. Per conversation it keeps a queue of sends (file count each) and the newest
  message id loaded when the first was handed over. Open chat: a send is done when its files
  show among `OwnThisDevice` rows newer than that id (whole sends only, oldest first). View at
  the newest messages but not reaching back to the send: the count ends. Chat not open: the
  first `MessagesChanged` with a newer id ends it. A lagged channel clears it. Pasted files
  (`$XDG_RUNTIME_DIR/kourier/media/paste`) are kept at least `PASTED_GRACE` (120 s) after
  their send and for as long as files are in preparation for that conversation
  (`ComposerController::release_sent_pastes`).
- §8.3 timeline (UI): `Block::Album` groups neighbouring pictures and videos of one message
  (a file ends the group); `timeline/blocks/AlbumBlock.qml` draws 2, 3, 4 or 5 tiles and "+N"
  beyond five (`ALBUM_TILES`); the grid is always `imageMaxWidth` wide, centred when a long
  caption widens the bubble. `ImageViewer` takes `items` and `index` and steps through the
  whole album (Left/Right, Page Up/Down, Home/End, buttons, a "2 of 9" counter). The messages
  of one send appear in order, each with its own state.
- Limits audit (user decision of 2026-10-06, CLAUDE.md "Rules"): every limit in the three lanes
  was checked for who enforces it. One table; "not copied" means Kourier never had it and
  Desktop does.

  | Limit | Where | Enforcer and source | Kept or dropped |
  |---|---|---|---|
  | Files in one send (32) | core `attachments/import.rs` `MAX_FILES`; UI `controllers::MAX_ATTACHMENTS` | nobody: Desktop's composer cap (`SD/ts/state/ducks/composer.preload.ts:1337`, `preProcessAttachment`) | Dropped: any number, the core splits |
  | One non-image file alone, no mixing of kinds | Desktop's composer (`composer.preload.ts:1343`) | nobody as a refusal; what receivers show is the next row | Not copied |
  | What one message carries: an album of pictures and GIFs, at most 31; every other file alone | `kourier-signal` `features::file_packing`, `ALBUM_MAX` | receivers: Android cuts at 32 pointers with the long text among them (`SignalServiceProtoUtil.kt` `toPointersWithinLimit`), Desktop at 32 (`processDataMessage.preload.ts` `ATTACHMENT_MAX`); Desktop shows only the first attachment unless all are pictures or videos it draws (`Message.dom.tsx`, `GoogleChrome.std.ts`), Android the first audio file or document (`ConversationItem.java`) | Kept as a packing rule: the send is split, nothing is refused. Phase I: the album is named by format and its size follows the remote config (the two rows added at the end) |
  | Attachments of a received message (over 32: the whole message was dropped) | `kourier-signal` `receive/dispatch.rs` `MAX_ATTACHMENTS` | nobody: Desktop and Android cut only for their own display; the server's 256 KiB message size bounds the list | Dropped: shown whole |
  | File size | `FileLimits.max_size`, `AccountCaps::max_attachment_bytes`; `features::max_plaintext` | server: `global.attachments.maxBytes` on the encrypted, padded upload (upload form `uploadLength`; `SD/ts/types/AttachmentSize.std.ts` `isAttachmentTooLargeToSend`; Android `maxAttachmentSizeBytes`) | Kept and made exact (102,224,512 bytes for 100 MiB) |
  | Desktop's video size (`global.videoAttachments.transcodeTargetBytes`) | Desktop | nobody: the target of Desktop's transcoder | Not copied |
  | Picture quality level one (1600 px, 1 MiB) | `kourier-signal` `send::media::PICTURE_LEVEL` | nobody: the default of Desktop's composer (`SD/ts/util/scaleImageToLevel.preload.ts`); official clients send level three on "High" | Dropped: level three; metadata is still stripped (privacy) |
  | "Dangerous" file types by name | Desktop (`SD/ts/util/isFileDangerous.std.ts`) | nobody on send; a Desktop receiver shows such a file but will not save it (`conversations.preload.ts` `saveAttachment`) | Not copied: nothing in Kourier refuses by file name |
  | Kind of file the chat takes | core import (`FileRefusal::KindNotAccepted`, kind read from content); UI `attachKinds` | network: `ConversationFeatures.files` | Kept in the core and for "no attach button where a chat takes no files". Dropped in the UI: the pre-check by file name, the paste gate, the picker's extension filter as a gate |
  | The same file twice in the composer | UI `ComposerController::add_file` | nobody | Dropped |
  | Empty file | core `FileRefusal::Empty` | nobody: Desktop's composer checks only the upper size, its receive path needs a numeric size (`processDataMessage.preload.ts`) | Dropped: sent as a 0-byte attachment (phones unverified); the variant is removed in phase I |
  | One refused file refuses the whole send | core, UI | Kourier's own | Adapted: the core names the refused files, the viewmodel sends the rest |
  | Voice note with a caption | `FileLimits.caption` (Signal: false for `Voice`) | weak: no client can edit a voice message and no official client sends one with a body | Kept as a split: the text goes first as its own message |
  | Caption with a kind that takes none | core `SendFiles`, `EditMessage` | network: `features.files` | Adapted on send (the caption goes first as a text); kept on an edit |
  | Text size 64 KiB, 2 KiB inline | `max_text_bytes`; `kourier-signal` `MAX_TEXT_BYTES` | receivers fetch at most 64 KiB of long text (`SD/ts/util/longAttachment.std.ts`, `AttachmentDownloadManager.preload.ts`) | Kept per message. Adapted: a longer `SendText` or caption becomes several messages; a longer edit is refused (one message) |
  | Empty message, unchanged edit | core `actions.rs` | receivers drop a message without content; an identical edit only uses up an edit | Kept |
  | Picture decoded in the import (64 MiB) | core `MAX_PICTURE_BYTES` | memory bound | Kept: a larger picture is sent without thumbnail and blurhash |
  | Incremental MACs per message (10) | `kourier-signal` `send::media::MAX_INCREMENTAL_MACS` | server's 256 KiB message size (Desktop `MAX_INCREMENTAL_MAC_ATTACHMENTS`, Android `maxIncrementalMacsPerEnvelope`) | Added |
  | GIF flag | `kourier-signal` `send/media.rs` | protocol meaning: a looping video (Desktop `isGIF` needs a video type) | Changed: only on video content |
  | Edits per message (10) | `kourier-signal` `features::MAX_EDITS` | iOS drops edits after the hundredth (`EditManagerImpl.swift` `maxReceiveEdits`); Desktop and Android do not count on receive (`handleEditMessage.preload.ts`, `EditMessageProcessor.kt`); 10 is each composer's own cap | Changed to 100; none in Note to Self |
  | Edit send window 24 h | `features::edit_send_secs` | receivers: 48 h after the original (Desktop `isTooOldToEditMessage`, iOS), on Android the delete window plus a day (`MessageConstraintsUtil.kt` `RECEIVE_THRESHOLD`); an accepted edit may wait 24 h in the outbox | Kept; capped by the server's delete window when that is shorter |
  | Edit receive window 48 h, any count | `edit_receive` | Desktop and iOS apply it | Kept; measured from the original's server time |
  | Edits of bodies that are not a text or a caption | core `actions.rs` and ingest (by target); `kourier-signal` drops edits that are themselves view-once, contact-share or voice messages | every client drops edits of view-once, contact-share and voice messages (`handleEditMessage.preload.ts`; Android `validTarget`; iOS `checkForValidEdit`, which also refuses stickers and polls) | Kept as a drop on receive |
  | Edit window, count and `features.edit` on an in-place edit of a message nobody has | core `actions.rs` | nobody: nothing of it reaches the network | Dropped |
  | Delete for everyone window | `delete_for_everyone`, `delete_receive` | server `global.normalDeleteMaxAgeInSeconds` (1 day); receivers a day longer (`SD/ts/util/canDeleteForEveryone.preload.ts:228` `MESSAGE_SEND_GRACE_PERIOD`, Android `RECEIVE_THRESHOLD`) | Kept; delete for me has no limit |
  | One reaction per user; a reaction is one emoji of at most 64 bytes | `reactions_per_user`; core `MAX_EMOJI_BYTES`; `kourier-signal` `emoji.rs` | protocol (receivers replace) and receivers (anything else is dropped); 64 bytes is Kourier's own security bound (the longest recommended sequence is 35 bytes) | Kept; the bound now also applies to incoming reactions |
  | Reply or reaction to an own message nobody has | Desktop's menu (`canReplyOrReact`) | nobody | Not copied: queued behind the message |
  | Disappearing timer values | `features.disappearing`, `SetConversationTimer` | nobody below 4 weeks: the list is Desktop's menu, the backend takes any whole second up to 4 weeks | Kept for now: a contract gap (a list, no range), open. Phase I: the gap is closed (`disappearing_custom`, a "Custom Time…" dialog); the four-week bound is a row of its own at the end. Phase J: that bound is dropped, the row at the end says what replaced it |
  | Typing notices | core `SetTyping`; `kourier-signal` `commands.rs` | privacy (Desktop `sendTypingMessage`, `shouldSendToConversation.preload.ts`); the cadence 10 s, 3 s, 15 s is what other clients expect | Added: none to blocked, unaccepted or unsendable chats or Note to Self; cadence kept |
  | Receipts | `kourier-signal` `commands.rs` | privacy: the setting and accepted chats; 100 timestamps per message and 500 ms are batching | Kept; none to blocked contacts. Phase I: also in groups, per author (row at the end) |
  | Send budget 24 h, backoff up to 15 min | core `outbox::BUDGET`, `MAX_BACKOFF` | the other clients' receive windows are built on it (`SD/ts/jobs/conversationJobQueue.preload.ts:339` `MAX_RETRY_TIME`); pacing toward the server | Kept: nothing is refused, the row stays for Retry. The wait for a challenge answer now counts against it. Phase I: only the part of the wait up to the challenge's own end counts, an overdue row keeps its one late attempt, and the wait for a safety number counts too (§4.8 step 3) |
  | Parked modifiers 2 and 7 days, chain depth 16, expired-message memory 45 days | core `ingest/modifier.rs`, `expiry.rs` | storage bounds (Desktop's `syncTasks` and recursion limit, the server's 45-day retention) | Kept. Phase I: the 45-day memory also holds the messages a chat clear removed |
  | Auto-download 25 MiB, long-text join 256 KiB | core `attachments/policy.rs` | Kourier's download policy: bounds what a sender can make the app fetch | Kept: a larger file downloads on click |
  | Receive size | `kourier-signal` receive | resource protection: `global.attachments.maxReceiveBytes`, else 1.25 × the send limit | Kept |
  | Received data: file name 255 bytes, blurhash 128, waveform 100 bars, avatar 10 MiB, contact-sync blob 64 MiB, HTTPS body 64 MiB | `kourier-signal` | security | Kept |
  | Retry requests: 5 per message, one session reset per hour | `kourier-signal` `retry.rs` | protects the peer from resend storms (Desktop's values) | Kept |
  | Device name 50 UTF-16 units | `kourier-signal` provisioning | unverified: Desktop's comment names 256 encrypted bytes on the server (`SD/ts/types/InstallScreen.std.ts`) | Kept as a cut, not a refusal |
  | Pacing inside the core: 4 transmits in flight, 5 s send timeout, sweep batch 200, 60 s sleep cap, 2 derive workers, 5 download resends, history batch, event queue | core | none refuses a user request | Kept |
  | Conversation list: the first 500 chats | viewmodel `CONVERSATION_LIMIT` | Kourier's own | Dropped: `CONVERSATION_PAGE` is a page size, every page is read |
  | Timeline window 500 rows and page 50; 5 album tiles; previews clipped at 256, 160 and 300 characters; six quick reactions | viewmodel; core `PREVIEW_MAX` | memory window with paging, layout, display | Kept: nothing is unreachable (every picture is in the viewer, the whole text in the chat) |
  | Composer previews only for eight raster formats by content; link schemes http, https, sgnl; https-only browser URL; a dropped web link is not fetched | `kourier-qt` `media.rs`; viewmodel `richtext.rs` | security and privacy (§8.6, §9.4) | Kept |
  | URIs from other processes: 16, each 4096 bytes | `kourier_desktop::instance` `MAX_URIS`, `MAX_URI_LEN` | security bound on input from D-Bus and the command line | Kept; the length is now 16 KiB (a `signalcaptcha` link carries the captcha token, about 2,600 bytes with no documented maximum; Desktop sets no limit on that route) |
  | A pasted file is deleted 120 s after its send | `kourier-qt` `controllers.rs` `PASTED_GRACE` | Kourier's own timer, tuned for 32 files | Changed: at least 120 s and for as long as files are in preparation for that conversation. Phase I: "in preparation" is known from the send's request id (`FilesStored`), no longer guessed from timeline rows |
  | dev-control: `attach` took one existing file per call, `send` needed a text | `kourier-qt/src/dev` | test tooling | Dropped |
| Formats that share an album: `image/jpeg`, `image/png`, `image/gif`, `image/webp`, `image/bmp`, `video/mp4` (phase I) | `kourier-signal` `features::ALBUM_TYPES`, `file_packing` | receivers, by the pointer's content type: Desktop draws an album only when it can draw every item (`SD/ts/util/GoogleChrome.std.ts` `isImageTypeSupported`, `isVideoTypeSupported`; `ImageGrid.dom.tsx`), iOS discards the album for an item that is not visual media (`CVComponentState.swift` `buildMediaAlbumItems`, `MimeTypeUtil.swift`), Android `SlideDeck.getThumbnailSlides` | Kept as a packing rule: every other format travels alone, nothing is refused. MP4 videos now share albums with pictures; HEIC, HEIF and AVIF no longer join them |
| Album size min(`android.attachments.maxCount` − 1, 32), at least 1 (phase I) | `kourier-signal` `features::album_max`; the key is kept from the remote config by name (`maintenance::KEPT_NAMES`) | receivers cut: Android by that key, Desktop at its constant 32 | Kept as a packing rule; a changed value re-emits every chat's features |
| Custom timer 1 s to 2,147,483,647 s (phase I: 1 s to 4 weeks; changed in phase J) | `kourier-signal` `features::MAX_TIMER_SECS` (`i32::MAX`), `TIMER_RANGE`; `timer::change` | interop, for the new bound: Android reads the `uint32` wire field as a signed `Int` (`SignalService.proto` `optional uint32 expireTimer`, `SignalServiceProtoUtil.kt` `(expireTimer ?: 0).seconds`, `ExpirationUtil.java`: `<= 0` is off), so a larger value shows there as "off" and never expires. Nobody for four weeks: it is where the three official pickers end (`DisappearingTimeDialog.dom.tsx`, Android `CustomExpireTimerSelectorView.kt` `WEEKS(1, 4)`, iOS `DisappearingMessagesCustomTimePickerViewController.swift` weeks `1...4`), no server sees the field, and Desktop, Android and iOS each store, show and run a longer timer they receive without a check (sources in §13 phase J and in the module doc of `features.rs`) | Four weeks dropped under the limits rule; the signed 32-bit bound kept. A value above it, below 1 s or with a sub-second part is refused (`InvalidInput`). The evidence is from sources, not devices: a timer above four weeks on real Android and iOS stays on the manual list |
| Read and viewed receipts in groups only for accepted groups and authors (phase I) | `kourier-signal` `commands.rs` | privacy (Desktop `sendReceipts.preload.ts` `shouldSendToDirectConversation`, `markConversationRead.preload.ts` `isConversationAccepted`) | Added |
| Anchors of a chat clear: the 5 newest messages (phase I) | `kourier_backend_api::CLEAR_ANCHORS` | protocol: every Signal client names its five newest addressable messages (`SD/ts/sql/Server.node.ts` `getMostRecentAddressableMessages`) | Added |
| Deletes for me and receipts that wait for their message or sender: 2 days (phase I) | core `ingest/modifier.rs` `PENDING_SHORT_MS` | storage bound (Desktop's sync-task retention, `SD/ts/sql/Server.node.ts:2556`) | Added |
| A stored challenge older than 24 h is not taken up at start (phase I) | core `outbox::StoredChallenge::is_fresh` | Desktop's `DEFAULT_EXPIRE_AFTER` (`SD/ts/challenge.dom.ts`), which is also every send's budget | Added: the next send finds out |
| Download resume: 5 requests in a row without a byte, pauses 1 to 8 s, 200 requests per download (phase I) | `kourier-signal` `attachment.rs` (`MAX_RESUME_PAUSE`, `MAX_DOWNLOAD_REQUESTS`) | resource bound | Added: ends as a retryable failure; the next download starts at byte 0 |
| Upload stall 60 s, tail 5 min (phase I) | `kourier-signal` `upload::STALL_TIMEOUT`, `net::runner::UPLOAD_TAIL_STALLS` | pacing | Added: refuses nothing, the upload resumes |
| Pacing added in phase I: 64 messages per enqueue transaction (`outbox::ENQUEUE_CHUNK`), import fsync batch 1024 (`import::SYNC_BATCH`), 16 derive results per commit (`derive::GROUP`), import progress every 100 ms, download progress every 250 ms, notification rest 250 ms per conversation with the first message shown at once (`policy::COALESCE`, `Pace`), hint-bus reconnect 500 ms doubling to 60 s (`hints::RECONNECT_FIRST`, `RECONNECT_MAX`), a trim 5 s after the first frame (`STARTUP_TRIM_AFTER`), the 50 ms poll for the released window, the fake's 4 parallel downloads | core, `kourier-signal`, `kourier-app`, `kourier-desktop`, `kourier-qt`, `kourier-fake` | none refuses a user request | Added |
| The composer asks for the exact count of a caption's messages only above 500 characters (phase I) | `kourier-qt` `Composer.qml` `captionWorthTelling` | Kourier's own pre-filter; right while `max_text_bytes` is at least 2000 (Signal and the fake: 64 KiB) | Added: a smaller limit would undercount the summary line, not the send |
| A second question before more than 1,000 files of a folder join the composer's list (phase J) | viewmodel `files::LARGE_ADD` | nobody: Kourier's own guard against a slip (a project folder whose every file would become a message) | Added as a question, not a limit: after the second yes every file is added |
| A folder is offered as one .zip only when the chat takes plain files and the folder's files add up to at most the chat's limit for one (phase J) | viewmodel `FileStrip::folder_question`, `ArchiveRule` (from `features.files[File].max_size`, `UiSource::archive_rule`) | network: the chat's file size limit (the "File size" row). The sum of the files is the estimate of the archive's size and is checked before anything is packed (user decision, 2026-10-07) | Adapted: "Add the Files Inside" is always offered. `AccountCaps::max_attachment_bytes` is not part of the pre-check (the viewmodel cannot see it; on Signal both limits come from the same remote value). An archive of data that does not compress can come out slightly over the limit and is then refused after packing; a folder that would compress to below the limit is not offered as an archive |
| Left out of a folder: names that start with a dot, symbolic links, sockets, pipes and devices (phase J) | `kourier_app::folders` (the walk, `Skipped`) | privacy and security (user decision, 2026-10-07, for hidden names and links): what the file manager hides was not seen when the folder was picked, and a link can lead outside the folder | Added: counted and named in the question. Each such file can still be picked by itself |
| The file name of a folder's archive is cut at 200 bytes (phase J) | `kourier_app::folders::NAME_MAX`, `archive_name` | file system (255 bytes per name; room is left for a number and `.zip`) | Added as a cut, not a refusal |
| Pacing added in phase J: the wait for a system bus that was absent at start doubles up to 10 min (`hints::ABSENT_MAX`), a second heap trim 2 s after the window's release (`RELEASE_SETTLE`), folder count and packing progress every 80 ms (`folders::PROGRESS_EVERY`), Signal's logout 7 s of network time of which 4 s for a login to settle, and 3 s for a login at shutdown (`LOGOUT_NETWORK`, `LOGOUT_LOGIN_SETTLE`, `SHUTDOWN_LOGIN`) | `kourier-desktop`, `kourier-qt`, `kourier-app`, `kourier-signal` | none refuses a user request | Added |
  | History transfer archive (step H0/H1): the file at most 8 GiB, a frame 4 MiB and 100,000 fields, 8 GiB decompressed, 20 million frames | `kourier-signal` `transfer::MAX_ARCHIVE_BYTES`, `archive` reader limits | security: the file is downloaded, and libsignal's reader allocates by lengths it reads | Added: a larger archive fails as "too large", nothing of it is kept |
  | History transfer: the wait for the phone 1 h, 5 min per long-poll | `kourier-signal` `transfer::WAIT`, `POLL` | Desktop's values; the phone gives up on its side | Kept |
  | History transfer: the download as a whole 2 h of running time, never under 4 KiB/s on average for 10 min after the first minute, 1 h without a byte | `transfer::DOWNLOAD_DEADLINE`, `MIN_DOWNLOAD_RATE`, `RATE_GRACE`, `SLOW_FOR`, `STALL` | Kourier's own: receiving is held back while it runs, and a body that trickles must not hold it back for days | Added: fails as "download"; the time the application was closed does not count |
  | History transfer: starts of the read 5, counting only reads that ended with the process | `transfer::MAX_READ_STARTS` | Kourier's own: a read that ends the process every time must not hold receiving back at every start | Added |
  | History import: people and chats kept while the archive is read (200,000 recipients and 128 MiB by estimate, 100,000 chats) | `kourier-signal` `transfer::map::MapLimits` | Kourier's own: memory, against a broken or hostile archive | Added: the frame is not kept and counted; what names it is unknown |
  | History import: the stage at most 8 GiB of payload, written in transactions of 16 MiB | `transfer::stage::MAX_BYTES`, `COMMIT_EVERY` | Kourier's own: disk, and the stage's own WAL | Added: fails as "too large", nothing kept |
  | History import: a text 128 KiB, a person's names 256 bytes each | `MapLimits::max_text_bytes`, `max_name_bytes` | the text: the format's own cap (libsignal's validator, which the reader does not run); the names: Kourier's own | Added: cut on a character, counted |
  | History import, per message: a quote's text 4 KiB, 2,048 formatting and mention ranges, 1,000 reactions, 100 ids of later versions, a placeholder's raw item 64 KiB | `MapLimits::max_quote_bytes`, `max_ranges`, `max_reactions`, `max_edits` (`MAX_HISTORY_EDITS`), `max_raw_bytes` | the edit ids: the contract's (the core reads 100); the rest Kourier's own: memory and stage size against a broken or hostile archive | Added: cut (the newest reactions and versions stay) or left out, the row stays; a range that crosses a cut text is shortened to the cut; counted (`quote_text_cut`, `ranges_cut`, `range_dropped`, `reactions_cut`, `edits_cut`, `raw_too_large`) |
  | History import: a time more than 24 h ahead of the import is not used: the sort time becomes the received time (else the import's), the row has no server time, a timer's start is the import's time. A disappearing timer is at most `i32::MAX` seconds | `MapLimits::max_ahead_ms`, `Mapper::timer_start`, `map::MAX_TIMER` | Kourier's own: a future sort time pins a chat to the top, and the core measures the edit and delete windows from the server's time. Up to the bound the phone's time stands, also for a timer's start: this computer's clock may be the one that is behind, and a start moved back to it would make a row due early. The timer bound is the live path's | Added: the id keeps the true time; counted (`future_time`, `future_server_time`, `future_timer_start`) |
  | History import, a delivery row: at most 500 messages (`MAX_HISTORY_BATCH`) and 1 MiB of staged bytes; a larger message travels alone. People 200 and chats 50 per row | `kourier-signal` `transfer::deliver::MAX_ROW_BYTES`, `DeliveryBounds` | the contract (500: the core refuses more); the megabyte and the row sizes are Kourier's own: one core transaction, and small first rows so the list stands at once | Added |
  | History import: at most 2 delivery rows unacknowledged | `deliver::WINDOW` | Kourier's own: `backend.db` stays small however large the history | Added |
  | History import: a chat frame's 1:1 timer at most `i32::MAX` seconds (less than a second is none), its version at least 1; a mute of `i64::MAX` ms or more is for ever | `transfer::map::chat_frame`, `storage::muted_until` | the timer bound and the mute's meaning are the live path's and the Storage Service's (one conversion for both) | Kept |
  | History import, per group: 4,096 entries of each member list (members, invited, requesting), a title of 1,024 bytes, 64 notices per group change (after folding) | `transfer::map::MapLimits::max_members`, `max_title_bytes`, `max_group_notices` | Kourier's own: memory and stage size against a broken or hostile archive (the server's group size is far below) | Added: the rest dropped or cut, counted (`members_cut`, `title_cut`, `group_notices_cut`) |
  | History import: at most 1,000 chats stay pinned: the first 1,000 in the phone's order (`pinnedOrder` ascending), the rest stay chats without the pin | `MapLimits::max_pins`, `map::reason::PINS_OVER_CAP` | Kourier's own: the pin list goes to the core as ONE event and is applied in one transaction, and Signal's clients pin only a few chats | Added: the chats beyond it are counted (`pins_over_cap`) and imported unpinned |
  | History import: at most 1,000 attachments per message | `MapLimits::max_attachments`, `map::reason::ATTACHMENTS_CUT` | Kourier's own: memory and stage size against a broken or hostile archive (Signal's clients send far fewer per message) | Added (S5): the first 1,000 kept, the cut counted once per message (`attachments_cut`) |
  | History import: an attachment is fetchable only while its upload (else original sent) time is within 2 × max(queue time, 30 days) of the phone's export time, and not above the receive limit | `transfer::map::MediaRules`, `Mapper::place` | Interop: what Signal Desktop says "could still exist on the transit tier"; the receive limit is the server's | Added (S5): the rest is "Not available on this device" with no pointer, counted (`attachment_too_old`, `attachment_too_large`) |
  | History import: a row found unacknowledged at more than 3 delivery starts, and the oldest waiting row after 10 minutes of RUNNING time without any acknowledgement, is taken back (a row of several becomes singles, a single is dropped and counted) | `deliver::MAX_STARTS`, `IDLE`, `strike` | Kourier's own: a delivery must not hold receiving back for good; the core stops at the first failing job, so only the oldest row can be at fault | Added |
  | A job that met a refused group credential (401 of the group server) is dropped after 30 attempts (about four hours of backoff) | `jobs::GROUP_AUTH_ATTEMPTS`, `jobs::gives_up` | Kourier's own: the group refresh jobs have no expiry, and a credential that is never accepted must not be asked for every fifteen minutes for good (Desktop fails and tries again at its next update, without a bound) | Added: the next start queues the refresh of an imported group that was never fetched again |
  | History import ends as "incomplete" after 3 takings back in a row without an answer, after 20 single units refused in a row, after 60 minutes of stuck running time in all, or after 3 chunks in a row out of step | `deliver::MAX_STRIKES`, `MAX_REFUSED_RUN`, `STUCK`, `MAX_OUT_OF_STEP` | Kourier's own: receiving is held back for a bounded time whatever the core cannot store; a fault must not loop | Added: what was delivered stays and receiving starts. A dropped unit alone leaves the transfer done |
  | Asking the phone for a lost file: the answer is awaited 30 s after the request went out, and 2 min more after each answer that still has a PENDING slot | core `Timeouts::restore_answer` / `restore_pending` (`ingest::restore::tune_waits`) | Kourier's own (Desktop has no wait: it shows nothing until an answer); a phone that never answers must end as a note the user can act on | Kept, own |
  | A backfill request that cannot go out is given up 1 h after the click, also before its first attempt; the core ends the wait itself 5 minutes later when nothing was reported | `kourier-signal` `backfill::REQUEST_MAX_AGE`, `jobs::expired_before_run`; core `ingest::restore::UNSENT_BOUND_MS` | Kourier's own: a request the phone gets hours later finds a user who gave up; the core's wait ends as "did not answer" (`NotSent`, or its own deadline when the backend never says) | Kept, own |
  | Backfill answers only from device 1 | `receive/dispatch.rs` `backfill_response`, `PRIMARY_DEVICE` | Kourier's own (security): Desktop and Android take one from any of our devices; only the phone has the files | Kept, own |

  Not limits: the sticker send refusal (a missing feature), sender key needing two recipients
  (a path choice; there is no group size limit), unknown `DataMessage` flags and newer protocol
  versions (protocol rules), dropping what a blocked sender sends (the receiver's rule, as in
  Desktop). Caches and rotation (`AVATAR_HANDLES`, notification `MAX_TRACKED`, 7 log files of
  10 MiB) limit nothing the user does. `ConversationFeatures.hard_to_save` (phase I: the 47
  extensions Desktop shows but will not save, `SD/ts/util/isFileDangerous.std.ts`) is a hint
  for the composer; such a file is sent under its own name. The UI has no rule of its own for edit, delete, react or
  reply (`MessageRow.actions` only) and no length limit on the composer text.
- Bugs fixed, core. (1) A modifier that states a conversation resolved account-wide under
  `AccountAuthor` scope, so an edit, delete or reaction sent in another chat with the right
  author and timestamp landed on the message; the rule is now §4.9's. Such a modifier is parked
  instead of dropped, because dropping made arrival orders diverge when a contact's two direct
  chats merge later; `Apply::retry_pending` runs at the end of `identity_merged`. The Signal
  backend states `conv: Some(..)` for edits, deletes and reactions and `None` only for receipts
  and read syncs, and drops group content from non-members, so backend plus core give Desktop's
  property (`isMessageAMatchForReaction`, `isValidSenderAciForConversation`). (2) `delete()`
  ignored an admin delete synced from our own phone (`by: None` on another member's message).
  (3) "Retry" reset `outbox.attempts` to 0, so a message whose send gave up on time-outs
  counted as "nobody has it" again: a delete for everyone then took it back silently and sent
  nothing. (4) `ConversationCleared` deleted the tombstone of a message whose delete for
  everyone was still queued; `outbox.message_id` cascades, so the delete was never sent.
  (5) `merge_conversation` ignored `conversations.hidden`: a chat with messages that merged
  into a hidden one vanished from the list. (6) A caption above `max_text_bytes` sent with
  files failed at the backend. (7) The unverified-clock set kept the id of a removed account
  (`accounts.id` can be reused).
- Bugs fixed, Signal. An album with a video could lose all but its first item on Signal
  Desktop (the packing rule above). A received message with more than 32 attachments was
  dropped whole. `image/gif` files carried the GIF flag. An oversized file failed at the
  upload instead of before the copy. The receive lane downloaded the long text of a blocked
  sender's message before the zone dropped it (`receive/media.rs` now skips those rows). An
  album of videos could exceed the server's 256 KiB message with its incremental MACs.
- Bugs fixed, UI. The conversation list showed only the first 500 chats. The list threw away
  every read that a change overtook, so during a sync it did not update at all: a list read by
  one query is now always shown (and read again when overtaken); a read over several pages
  that is overtaken starts over at most `LIST_RESTARTS` (2) times in a row, then is read to its
  end, shown and read once more. A second request for the same edit history or the same file
  plan left its query tracker waiting for ever (`QueryTracker::reset` now frees it when a
  follow-up has nothing to ask; `TimelineView.openHistory` opens one dialog per message).
  "Preparing N files…" could stay for ever when a send became more messages than the loaded
  page. A pasted picture was deleted 120 s after its send while the core was still copying, so
  the whole send was refused. A second refused send overwrote the first one's returned files.
  A `signalcaptcha` link longer than 4096 bytes was cut off. In the narrow layout the pictures
  of an album overlapped the caption.
- §4.2 contract additions: `BackendEvent::ConversationCleared { conv, up_to, remove, at }` and
  `BackendEvent::AttachmentDeleted { target, net_ids: Redacted<Vec<String>>, at }` (both
  durable, both checked by `Envelope::validate`, neither is mirrored state);
  `Attachment.net_ids: Vec<String>` (serde default, redacted in `Debug`; a full struct literal
  of `Attachment` needs the field); `AccountReady.number: Option<Redacted<String>>` (serde
  default, skipped when `None`; `None` leaves what the core has; set at the link and again,
  with the new `own_ids`, after a number change); `ErrorKind::Challenge.retry_after:
  Option<Duration>` (serde default, also returned by `ErrorKind::retry_after()`);
  `BackendNotice::ChallengeRequired { url, retry_after }`. `BackendEvent::Edited` now states
  who checks what (§3.5.3, §3.8), and `Edited.edited_at` and `Deleted.at` are the server's time
  where the network gives one, so backends set `IncomingMessage.server_at` wherever the network
  dates a message. `CoreError::Backend` holds `Box<BackendError>` (`From<BackendError>` kept).
- Facade (`kourier_core::facade`): `Query::FilePlan(FilePlanQuery { conv, paths, caption: bool,
  reply: bool }) -> QueryData::FilePlan(FilesPreview { kinds: Vec<Option<AttachmentKind>>,
  refused: Vec<RefusedFile>, plan: FilePlan })`: what `SendFiles` would do right now; the files
  are only inspected, never copied; a caption counts as one message at most. `RefusedFiles`
  (the subject of a refused `SendFiles`) gained `refused: Vec<RefusedFile { index, reason:
  FileRefusal }>`, ascending by position, empty when the refusal was not about a file; every
  other path can be sent. `FileRefusal::{ Unreadable, NotAFile, Empty, TooLarge { max, chat },
  KindNotAccepted { kind } }`: `chat` tells the conversation's per-kind limit from the
  account's; `Empty` is never produced any more (removed in phase I). `Query::Revisions(MessageId) ->
  QueryData::Revisions(Option<MessageRevisions { message, conv, revisions: Vec<Revision {
  revision, text, at }> }>)`: oldest first, the last entry is what the row shows, one entry
  for a message never edited, none for a deleted one, `None` for an unknown message; `text` is
  the text or the caption of that version. `AccountRow.number`. `facade::CoreError` is
  re-exported (the viewmodel classifies errors by kind, and `deps-check` allows it only
  `kourier_core::facade`). `MessageActions.take_back` is false for a message that waits for a
  retry or whose send gave up without a definite answer; `MessageActions.edit` is true with
  `until: None` for a message nobody has. Error texts of `SendFiles`: new "this chat cannot
  quote a message with files and no text"; gone "too many files in one message", "this chat
  takes no caption with this kind of file" and "a picture grew while it was read". A chat
  deleted on another device arrives as `ConversationsRemoved { ids }` plus
  `TimelineInvalidated`; it leaves `Query::Conversations` (inbox and archived) while
  `Query::Conversation(id)` still answers, and comes back with `ConversationsChanged`.
  `Attachment.net_ids` never reaches the UI (cleared in `MessageRow.body`, like `remote`).
- §4.9 ingest rules (core; `ingest/modifier.rs`, `actions.rs`): the stated conversation binds
  a modifier, parked ones are tried again at `IdentityMerged`; the core decides what can be
  edited (a voice note takes no new caption where the chat's `files` say so, but an existing
  one can be removed; an edit whose own body is neither text nor media is dropped); receive
  windows run from the original's `server_at`, else `sent_at`, as an absolute distance, as
  Desktop's `getMessageAge`; an incoming reaction above 64 bytes is dropped. The core does not
  check that the sender of a group reaction is a member: the backend does, at the envelope's
  group revision.
- §4.8 send rules (core; `outbox.rs`, `dispatch.rs`, `actions.rs`): the "may be with the
  server" rule of step 5 (`actions::sending_of`); the per-member identity pause of step 3 (the
  resend goes to the waiting members only, with the same id, as a recipient update; the same
  for edit, delete and reaction rows, which are not rolled back); the challenge rules of step 3
  (`outbox::give_up_overdue`; `Dispatcher::kick` returns `Kicked { changes, failed }` so rows
  that give up are reported as `ActionFailed`). A `BackendNotice::ChallengeRequired` ends on an
  accepted `SubmitChallenge` (`token: None` unless a send was challenged too), after its
  `retry_after` counted from when the core applies the notice, on a fresh `Online`, or on an
  actor restart. Ingest sets `Changes.outbox` when a tombstone, an echo or a clear removed
  outbox rows, and the supervisor kicks the dispatcher at once, so rows behind a removed head
  do not wait for its backoff timer. Typing is gated as in §4.8.
- §4.3 step 6 and §9.6 (core): the three commands kept from a linking actor, and the hold on
  the sweeper until the backend's first verdict. The hold has no time bound on purpose: a
  clock far enough ahead makes TLS fail, which shows as Offline, so releasing on Offline would
  defeat it in the worst case. It is a privacy trade-off the user has not decided yet (open
  list). (Phase J: decided. The hold is one of three options, `KeepUntilConfirmed`, and the
  default hides the due messages instead, §9.6.)
- §3.5.2, §6.13 delete for me of chats and attachments: the core rules are §4.9's
  (`ingest/clear.rs`); the anchor rule and the `remove` rule both leave a message that has a
  `Message` or `Delete` outbox row. The Signal mapping is in §6.13's table. Only receiving
  exists. The fake plays the phone in `scripts::delete_for_me()`. Desktop keeps a delete-for-me
  sync task whose target is not there yet and tries it again
  (`SD/ts/util/syncTasks.preload.ts`); Kourier applies the event once (open list). (Phase I:
  the deletes can be made in Kourier too, and an event that overtakes its messages is parked
  for 2 days.)
- §6.6, §6.7, §6.10 blocking and avatars (Signal): one drop point for everything a blocked
  sender's envelope held (`receive/dispatch.rs` `dispatch_content`, after decryption, so
  sessions stay in step), as Desktop's `MessageReceiver` does; nothing goes to a blocked
  contact (read, viewed and delivery receipts, typing). `preferContactAvatars` is honoured
  without a schema change.
- §4.6, §6.7, §6.12 challenges and number (Signal): `ErrorKind::Challenge` carries the
  Retry-After of the 428 from the REST path and from the typed path. A job that meets a 428
  announces the challenge, waits the Retry-After (a minute without one) and is due at once
  after an accepted answer; jobs now wait for any Retry-After, rate limits included (phase I:
  one notice per standing challenge and a hold on the other sending jobs, §6.12).
  `TransportError::Challenge` and its mapping exist for the multi-recipient send, but
  libsignal-net v0.104.0 has `UnauthenticatedChatApi::ALLOW_RATE_LIMIT_CHALLENGES = No`
  (`LS/rust/net/chat/src/api/messages.rs:151`), so the 428 arrives as an unexpected status and
  the send falls over to 1:1. `AccountReady.number` is set at link, at rebuild and after a
  number change.
- §4.7, §8.8 desktop side of S15 (`kourier-desktop/src/hints.rs`, `kourier-app/src/desktop`):
  `SystemHints` holds three match rules (logind `PrepareForSleep`, NetworkManager
  `StateChanged`, `PropertiesChanged` for `PrimaryConnection`); `PrepareForSleep(false)`
  becomes `UiCommand::Resumed`, the other two `UiCommand::NetworkChanged`; a suspend and other
  properties are ignored. Each stream has a queue of 16; a full queue holds the connection's
  reader back, no signal is dropped. `DesktopOptions.system_bus: SystemBus { System, At(addr),
  Off }` (`SystemBus::from_env`): the machine's system bus only in `DesktopMode::Auto`; with
  `KOURIER_DESKTOP=1` (the test set-up) only a bus named by `DBUS_SYSTEM_BUS_ADDRESS`, so no
  test listens to the real logind or NetworkManager; off with desktop services off and by
  default. The desktop file registers
  `x-scheme-handler/signalcaptcha` beside `sgnl` and passes `desktop-file-validate`; a
  `signalcaptcha://` link given to a second launch reaches the running instance and answers
  the challenge (tested with the real binary on a private bus and a 6 KB link).
- §8.3 other UI: the "edited" mark and a menu entry "Edit History" open
  `dialogs/RevisionsDialog.qml` (versions newest first, text escaped). Notices are chosen by
  `CoreError` variant, `ErrorKind` and the row's state; no error text is matched anywhere. The
  core's refusals are still `InvalidInput(&'static str)`, so "too old", "edited too often",
  "deleted" and "being sent" are told apart from the row (action flags, revision against
  `features.edit.max_count`, tombstone, Sending; phase I: the core states the reason,
  `CoreError::Refused { reason }`). Notices (system rows) have a context menu
  with "Delete…" by right click, long press or Menu key. The timeline is one tab stop: the
  toolbar buttons of a row no longer take Tab focus, Enter also opens the current row's menu,
  Up and Down stop on notices that offer something. Reaction chips sit below the bubble.
  `poll-ended` and `pin` have stand-in texts. The composer of a logged-out account has its own
  sentence, so the banner's is not repeated.
- Viewmodel and bridge API: `UiSource::{plan_files, take_file_plan, take_plan_failure,
  preparing_files, request_revisions, take_revisions}`; `RefusedSend { files: Vec<(PathBuf,
  Option<FileRefusal>)>, resent }`; `Changes` gained `plan` and `revisions`; `timeline`:
  `Block::Album`, `FileBlock.kind`, `ALBUM_TILES`; new module `files` (`FileStrip`, `FileRow`,
  `FileFacts`, `StripSummary`, `PlanRequest`, `PlanAnswer`, `refusal_text`, `refused_notice`).
  QML: `App.composer.files` (list model) replaces the attachments variant list;
  `addFiles(list)`, `addFile(url)` without kinds, `paste()` replaces `pasteImage(kinds)`,
  `setHasText`; composer properties `sendableCount`, `refusedCount`, `messageCount`, `planned`,
  `summary`, `hint`, `preparing`; `App.showRevisions`, `App.revisions`, `revisionsLoaded`;
  `App.findMessage` takes "album" and "edited"; `MediaActions.viewAlbum`;
  `TimelineActions.history`; `AttachDialog.autoOpen` (tests turn it off).
- Fixture and screenshots: pages `files`, `album`, `albumviewer`, `revisions`; `PAGES` and
  `SHOT_PAGES` hold 29 pages and `cargo xtask shots` renders 116 PNGs; `albumviewer` joins
  `viewer` in `SHOT_PAGES_SAME_IN_BOTH_THEMES`. The fixture has one more showcase chat ("Sol
  Vireo": albums of 4, 2, 3 and 9 pictures and a text edited twice), so 19 conversations;
  `FixtureInfo.albums_conversation`; the stand-ins chat gained `poll-ended` and `pin`.
- §10.2 fake: its packing rule is albums of `scripts::ALBUM_MAX` (3) pictures, GIFs and
  videos, other files alone (`scripts::file_packing()`, part of `scripts::signal_features()`;
  small on purpose, not Signal's rule). `scripts::delete_for_me()` with
  `delete_for_me_chats()`, `attachment_name(local, n)` and `DELETE_FOR_ME_PAUSE`.
  `Script::with_number`, `Script::fail_relink(n)` (the first n relinks fail with a `Failed`
  login step, counted per account per process), `challenge_error_after(url, Duration)`. Own
  timer changes count on from the highest scripted `ConversationPatch.timer_version`. A relink
  does not replay a script the account already ran (a script pushed with
  `FakeControl::push_script` for that start still plays; phase I: a relink plays the rest of
  a script the account had not finished). Both `kourier-app` workarounds of
  phase G are removed.
- Dev tooling and demo (debug builds only): dev-control `state` gained `rows` (the newest 200
  of the open chat, marks only: `message`, `own`, `state` none, pending, sent, delivered, read
  or failed, `edited`, `deleted`, `text`, `quote` null, known, missing or deleted,
  `attachments`, `kinds`, `reactions` of `{emoji, count, mine}`), `composer` (`files`,
  `sendable`, `refused`, `messages`, `planned`, `preparing`), `challenge` (the account that
  waits, or null) and `banner`; `media` has one entry per picture of an album. New command
  `challenge {answer}` (`notFound` when no account waits). `attach` takes `path` or `paths`,
  returns `{added, attachments}` and no longer refuses a missing file (it is marked once the
  core has looked). `send` may have no text when files wait. New `wait` conditions
  `challenge`, `no_challenge`, `none_pending`, `files_planned`. `KOURIER_DEV_DEMO_STATE=
  challenge|offline|clock` (`kourier-app/src/dev.rs`) sets the state the fake demo account ends
  in on a normal profile; fixture mode claims no single-instance name, so a second-launch test
  cannot use it. `cargo xtask demo` gained `files_scene` (two pictures and three documents in
  one send; it fails unless the composer announces four messages, the contact's phone gets the
  album and each document byte for byte in the picked order, and Kourier's timeline shows the
  four messages as sent), and `modifiers_scene` now also fails unless Kourier's own timeline
  shows the reaction, the edited mark, the quote and the tombstone. 35 screenshots.
- §10.3 harness: new command `challengeMultiRecipient {token?, retryAfter?, stop?}` (428 with
  Retry-After on multi-recipient sends over the websocket route); `sendSync.deleteForMe` gained
  `mostRecentNonExpiring`, `localOnly` and attachment names (`clientUuid`, `digest`,
  `plaintextHash`); `smoke.mjs` has cases for `sendData`, `sendSync`, `waitForEdit` and
  `challengeMultiRecipient` (20 cases). With multi-recipient sends kept on the websocket route
  and no challenge set, the mock answers its missing route with a server error.
- §10.2-§10.4 tests: core `tests/{files, delete_for_me, revisions, hostile, send_rules}.rs`,
  and in `outbox_crash.rs` a killed split send (13 kills) and a kill around an unconfirmed
  member (6 kills); `hostile.rs` runs every cross-chat case in both arrival orders and
  compares dumps. `robustness.rs` uses a small `ClockJudge` backend because the fake cannot
  open a start with `FixClock`. Since the sweeper waits for the backend's first verdict, a core
  test that restarts and expects a sweep sends `UiReady` first. Signal: `kourier-e2e`
  `tests/signal_files.rs` (the split in 1:1 and group, and a three-message send killed at
  `outbox_after_enqueue_commit:1`, `outbox_after_attempt_commit:2` and
  `outbox_before_outcome_commit:2`: every message once, four uploads per round) and
  `tests/signal_sync_state.rs` (attachment and chat deletes from the phone, a blocked contact,
  a challenge met by a receipt). `signal_modifiers` gives its old messages an old
  `serverTimestamp`, because the edit window is measured from the server time. UI:
  `kourier-viewmodel` `files.rs` and `core_source/files_tests.rs`; `kourier-qt`
  `tests/files_ui.rs` (`harness = false`, real core on the fake: 300 files added in one call,
  105 messages, 2 refused, order and caption checked in `core.db`); `kourier-desktop`
  `tests/hints.rs` (private `dbus-daemon`, mock logind and NetworkManager, an impostor that
  does not own the names); `kourier` `tests/single_instance.rs` for the `signalcaptcha`
  hand-off and the desktop file.
- Open for the next phase (what phase I closed is named in the last item; everything else here is
  still open and is carried into phase I's list):
  - Decided by the user on 2026-10-07 and built in phase J: the unverified-clock hold of the
    sweeper is a setting with three behaviours (§9.6), and a folder given to the composer asks
    whether to add its files or to send it as one .zip (§8.3).
  - Contract: an `AttachmentDeleted` that carries only `fallbackPlaintextHash` is not applied.
    Still open from phase G: `OutgoingKind::Edit` without `reply_to` and expiry, one
    `admin_delete` flag and one window for normal and admin deletes, `QuoteSnapshot.thumb`
    outgoing only.
  - Core: a Failed row that gave up while waiting for a challenge counts as "may be with the
    server" when it had an attempt (the cost is one delete sent for a message nobody has). A
    message whose send gave up is usually older than the edit and delete-for-everyone windows by
    then: it can only be retried or deleted for me. `ConfirmIdentity` during a link is not
    remembered for after the link. Still open from phase G: a long text over 256 KiB or empty is
    not joined; `AfterRead` timers started under a wrong clock; a held account that is logged out
    stays held; no test for the resend of pending commands at link time; the drain-path relink
    test relies on 300 ms of real time.
  - Signal: a 428 on the sender-key send cannot be reported as a challenge with libsignal-net
    v0.104.0: if a server challenged only that route, the message would go out 1:1 and no
    challenge would show (Desktop fails the send, `sendToGroup.preload.ts` `_shouldFailSend`);
    check again at the next libsignal bump (§6.15). `preferContactAvatars`: after a change the
    other picture shows only once the re-fetch arrives, both pictures are not kept, a photo
    removed from the phone's address book is not handled. Not verified from a source: the server's
    bound on the encrypted device name, whether iOS cuts a message's attachments on receive,
    whether the phones accept timers over four weeks (phase J read their sources: they do).
    Kourier's edit receive window is 48 h,
    Android's the delete window plus a day (differs only under a non-default server config). A
    0-byte attachment is untested on the mock's upload path and on the phones. `smoke.mjs` cannot
    observe `sendSync` on a scripted device. Still open from phase G: a group's timer cannot be
    changed (group writes); the `timer_update` job drops the peer send on a permanent error;
    contact-sync timers only from the initial sync; incoming call offers are dropped; group story
    replies and story reactions as `story-reply` placeholders (to decide); stage-1 backpressure
    does not serve the keepalive, hints or the unlink signal; the unlink drain is not cancel-aware
    and ignores commands; a Retry-After given at connect is cut short by a hint; a clock so wrong
    that TLS fails shows Offline, not `FixClock`; an unlink while offline or held is noticed only
    at the next connect and `Logout` without a socket cannot tell the server; the PNI identity
    check of obligation 13 and a test for the group credentials refetch after a number change; 1:1
    sends over raw REST; the mock copies the client timestamp into `serverTimestamp` in its own
    senders and hands a freed device id out again; the interactive demo has no react, edit or
    delete commands and its `photo` command and `inbox` attachment lines were never run. Still
    open from phase F: the incremental MAC is not verified on receive, sticker send, avatars
    written on the account thread, the pointer cache (not keyed by plaintext hash, keeps pointers
    of deleted messages), an unknown CDN number and recipients reported unregistered (both to
    decide), follow-up transcripts list only the members of that attempt, no endorsement cache
    across sends, group send tokens not `Zeroizing`, the view-once flow.
  - UI: the image viewer shows "Play" for a video, which opens in another application. The edit
    history lists caption versions only for a message with files. Tests: the `files` fixture page
    waits only for the plan, so on a loaded machine its screenshot can show attachments at "0%
    downloading". Still open from phase G: an edit with two separate changes is one stretch, and
    the composer cannot create or show formatting or mentions; the emoji grid has no search, skin
    tones or recents; the jump to an original that is not loaded is tested in the viewmodel only;
    no Orca pass. Still open from phase F: no in-app video player, voice notes through Qt
    Multimedia's content probing, a media copy left in tmpfs after a lagged channel, an avatar
    replaced in place under the same key, real audio output untested.
  - Real server and manual: the real-server smoke run needs a dedicated test account and the
    user's OK. From phases F and G: group send, attachments, upload resume on TUS and CDN 2,
    upload hosts under Signal's CA only, 401 or 403 for a removed device and the 4401 close,
    whether the server keeps one-time keys on an upload without them, the real 428 body with
    `options` and Retry-After, the answer to a wrong captcha and the captcha page flow, a silently
    dead TCP connection. From phase H: what Signal Desktop, Android and iOS really show for an
    album of 31, a lone video, a HEIC picture and an album behind a caption over 2 KiB (the
    packing rule is built from their sources, not from a run); a 0-byte file; a picture at level
    three; the Retry-After of a real 428 on sends and on jobs. Manual Plasma checks: drag and drop
    and paste of files from Dolphin, `Qt.openUrlExternally`, the "Open this file?" dialog, the
    real logind and NetworkManager signals (suspend and resume, a network switch), the browser
    handing a `signalcaptcha` link to Kourier (needs the desktop file installed and the MIME
    database updated), the §10.6 checklist.
  - Still open from phase E: bounded test parallelism for the mock suites (run by hand with
    `--test-threads 2`).
  - (Closed in phase I and removed from this list: `cargo xtask ci` and the demo ran on the merged
    phase H tree before its commits, and S16 was built. The user decided both questions (first
    item). Contract: packing that sees MIME types; names for the attachments Kourier itself sent;
    outgoing commands and UI actions for clearing a chat and deleting one attachment;
    `ConversationCleared` and `AttachmentDeleted` parked when they overtake their messages; the
    messages a clear removed are remembered; a timer range beside the list; a request id and
    "stored" and progress events for `SendFiles`; `Query::FilePlan` with the caption's text; typed
    refusal reasons; `FileRefusal::Empty` removed; `ConversationRow.hidden`. Core: the late
    attempt of an overdue row under a challenge; an announced challenge kept on show through a
    Retry; challenges kept across a restart; the safety-number wait bounded by the budget; rows
    behind a head the sweeper deleted; receipts from someone the message was not sent to; a partly
    failed message and a tombstone with a queued delete under a chat clear; the blob of a download
    that ends after its row went; a Retry after `AttachmentDeleted`; sends of thousands of files;
    `accounts.id` AUTOINCREMENT; an incoming edit without a quote; the long texts of a chat that
    stops being blocked or request-pending; the core's side of a `CancelLogin` or removal racing a
    link; `take_due` reading every waiting row. Signal: the identity merge emitted from every
    source that has the pair; the cross-chat cases in `kourier-e2e`'s hostile test; one notice per
    challenge, and an accepted answer that leaves a rate limit alone; read and viewed receipts in
    groups; "send as file"; the sender told about file types Desktop will not save;
    `android.attachments.maxCount`; downloads that resume and report progress; upload stall
    detection. UI: files kept per chat; the count of files in preparation; keyboard access to a
    row's picture, file, voice note and quote; the viewer following its message; the edit history
    refreshing while open; the account's number in the drawer; the three looks items and the list
    preview of a sent document; the hint watcher reconnecting; the timing and focus races of
    `files_ui`, `core_ui` and `input_ui`. Fake: a relink plays the rest of an unfinished script.)

Phase I (S16: the measuring tool, the first numbers against §1.3 and the fixes they asked for,
packaging and install; a contract step and three lanes that closed most of phase H's open list in
core, Signal and UI; 2026-10-07):

- Schema: `core.db` is version 8. `0008_account_ids_outbox_heads` is a rebuild step
  (`Step::Rebuild`, phase F): `accounts` gets an AUTOINCREMENT id with every row and id kept,
  because rows without a foreign key are kept per account id (the `settings` scopes
  `account:<id>`: the id allocator, the clock verdict, a challenge that waits) and so are sets in
  memory; an account removed before this migration can have had its id reused once. It also adds
  the partial index `outbox_waiting(conversation_id, id) WHERE state IN ('Queued',
  'Transmitting')` and the table `download_orphans(request, account_id, attachment_id, path,
  dropped_at)`. No migration for the rest: `outbox.state` has a fourth value `Staged`, a waiting
  challenge is a `settings` row, and `expired_messages` also holds the keys of messages a chat
  clear removed (the table's name and the comment of migration 0006 still speak only of expiry).
  Signal `backend.db` is version 12: `0012_challenged_jobs_identity_links` adds `jobs.challenged`,
  `jobs.held_from`, the table `identity_links(pni, aci, learned_at)` and the partial index
  `peers_e164`. The fake's `backend.db` gained `0003_fake_job_payload` (`jobs.payload`: the JSON
  `DurableCommand` of a delete made on this device).
- S16, the tool (`xtask/src/perf.rs`): `cargo xtask perf [--out <file>] [--quick] [--report-only]
  [--idle <s>] [--runs <n>] [--no-mock] [--no-build] [--keep-profile]`. It builds a measuring
  binary (cargo feature `perf`, which implies `dumpable` and `fake`; copied to
  `target/perf/kourier-perf`) and the normal release binary. It seeds the reference profile of
  §1.3 through the real core on the fake network (`Launch::Perf`, `kourier_app::perf`: 200
  conversations, 50,000 messages = 101,166 events, 3,654 attachments = 7,308 blob files, the
  history ending 10 minutes before now; about 40 s, and again whenever the measuring binary
  changed, so a schema change is never measured as a migration at start). It starts a private
  D-Bus session with a mock notification server and tray host (`kourier-desktop` example
  `mock_session`, one per app run) and runs the app offscreen on copies of the profile: 10 plain
  starts that are quit through the tray menu, 3 starts with Kourier's own files evicted from the
  page cache (`posix_fadvise`; the system's libraries stay cached), 5 starts with one live message
  each, and one scripted session (`crates/kourier-qt/perf/PerfDriver.qml`, bundled only with
  `perf`: 21 conversations opened, 8 s of flicking through an image-heavy chat, 300 s idle with
  the window shown, hidden to the tray and released, 300 s idle again; each idle time is sampled
  in 3 parts, so a burst shows as one busy part and a periodic wake-up in every part). Last it
  runs the `kourier-e2e` catch-up test on the Signal mock three times as a release build. Every
  child process gets its configuration, cache, data and state directories below `target/perf/env`,
  the bus of the private daemon and no system bus: nothing is read from or written to the user's
  home, and no real server is contacted.
- The tool's output and verdicts: one Markdown table (target, budget, measured, status, how it was
  taken and from how many runs) and the evidence behind it (smaps by mapping, how much of Qt6Core
  other processes map, heap in use and free, wake-ups per thread), on stdout and in
  `target/perf/report.md`. Status `ok`, `close` (the median is inside the budget but the worst run
  is outside, or the value is within 20 % of the budget), `MISS`, or `info` for rows that have no
  budget or measure another workload (the start with evicted files, the anonymous part of the
  memory, the scroll on the software renderer, the fake-network ingest with media, the binary
  size); `info` rows never fail the run. One extra enforced row checks that the plain release
  binary is non-dumpable. Exit status 0 when every budget is met (or `--report-only`), 3 when the
  measurements worked and a budget is missed, 1 when a measurement itself failed (a run that
  crashed or never reached a mark, a failed test), also with `--report-only`. `--quick`: 3 starts,
  10 s of idle, the window released after 3 s, no mock test (about 2 min after the builds); a full
  run takes 13 to 15 min, 11 of them the two idle times and the 60 s release delay. Nothing is
  built with less than 25 GB of free disk. On the phase I tree a plain run exits 3 (the two memory
  rows).
- What the tool depends on (a change there breaks `cargo xtask perf`, not the app): the QML names
  the scripted session reads (`window.started`, `window.chatPage`, `window.openConversation`,
  `window.hideToTray`, `window.pageStack.get(0).flickable`, `chatPage.timeline.rowList`); the tray
  item registering on the private bus (the measured starts are quit through it); the
  `mock_session` example's lines (`DBUS_SESSION_BUS_ADDRESS=`, `notify id=`, the command
  `tray-quit`); the core's `recv.commit` mark in the log (a run without it fails).
  `PerfDriver.qml` lies outside `qml/`, so `cargo xtask ci` does not lint it: run qmllint on it by
  hand after a change. A perf profile is not a fixture profile: `Launch::Perf` uses the system
  clock and time zone, claims no single-instance name, and on a seeded profile runs a script of
  empty steps (it relies on the fake's durable step cursor) instead of building the 50,000-message
  script again at every start.
- Trace marks. `kourier_app::trace` (`crates/kourier-app/src/trace.rs`) prints `kourier-trace:
  <name> <ms since start> key=number …` on stderr when `KOURIER_TRACE_STARTUP=1` is set, numbers
  only: `startup.app_started`, `startup.qt_app`, `startup.qml_engine`, `startup.qml_loaded`,
  `startup.first_frame`, `startup.list_populated`, `startup.trimmed`, `timeline.open`,
  `window.released`, `window.released_settled`, `notify.shown`, and `recv.arrived` (the fake's
  delivery instant, through a clock only perf builds have). The core and the Signal backend mark
  the receive path as `TRACE` events on the `tracing` target `kourier::perf`
  (`kourier_core::PERF_TARGET`, `kourier_signal::PERF_TARGET`), again numbers only: `recv.commit`
  (one envelope of durable events committed and acked: `account`, `events`, `messages`, `notify`,
  `catch_up`, `took_us`), `recv.catchup` (the backend left `Syncing`: `messages`, `conversations`,
  `took_ms`, `per_second`) and `recv.stored` (Signal stage 1, one event per batch: `envelopes`,
  `waited_us` from the first envelope's arrival at the actor to the inbox commit, so the 75 ms
  batching is in it, `backlog`). Nothing subscribes by default, so a mark costs one
  disabled-callsite check; they are read from the log with `KOURIER_LOG=warn,kourier::perf=trace`.
  QML hooks, inert without the variable: `App.traceFrame` and `App.frameTraced(listShown,
  chatShown)` (a `Connections` on `frameSwapped` in `MainWindow.qml` that is enabled only while a
  mark waits), `App.traceMark(name, "key=number …")`, `App.conversationAt(row)`,
  `App.perfScenario` (empty outside a perf build).
- S16, the numbers. Machine: i5-9600K (6 threads), release build, offscreen software renderer,
  window 1200x800, reference profile on the fake network. "First" is the first measurement, taken
  at the start of phase I with other builds running (load average 4.0); "final" is three full runs
  on the final tree with no other build running, one value per run. Run 3 was 8 to 15 % slower on
  everything CPU-bound with nothing changed but the machine's state (the same scroll took 3,597 ms
  of CPU instead of 3,123 to 3,128; governor `intel_pstate` powersave), so that spread is the
  machine, not the tree. The reports are generated files: `target/perf/report-phase-i.md` (run 3)
  with `-run1` and `-run2`, and `report-final.md` for the first measurement.

  | Target | §1.3 budget | First | Final, runs 1 / 2 / 3 | How it was taken |
  |---|---|---|---|---|
  | Cold start to first frame | ≤ 400 ms | 1,712 ms (worst 1,790) | 339 / 339 / 366 ms, slowest single start 377: met, status `close` | `main` → first `frameSwapped`, median of 10 plain starts after one that is not counted, warm page cache. Per start: `App::start` (keys, `core.db`, core) 36 to 43 ms, `Main.qml` loaded at 235 to 264 ms (first: 431 to 448), the list's refresh on the GUI thread 0 to 1 ms (first: 481 to 499) |
  | Start with Kourier's own files out of the page cache | none (the 400 ms shown for comparison) | not measured | 451 / 443 / 478 ms | the binary, the profile and Kourier's cache directory written out and evicted (7,350 files, 249 MB); median of 3 starts |
  | First frame to populated list | ≤ 300 ms | 0 ms | 0 ms in all 30 starts | the first frame already shows the rows |
  | Open a conversation | ≤ 100 ms | 52.8 ms (worst 64.6) | 50.3 / 50.7 / 56.4 ms, slowest 67.3 | span `timeline.open` (`App.openConversation` → the frame with the rows), 21 conversations in one session with 53 to 55 rows loaded; the query alone 4.1 ms; the first open, which creates the chat page, 55 to 61 ms |
  | Idle memory, window shown | ≤ 150 MB PSS | 252 MB (anonymous 171.5, file 80.0) | 157.8 / 157.7 / 156.5 MB (anonymous 86.8 to 86.9, file 69.6 to 71.0): MISS | `/proc/<pid>/smaps_rollup` of the measuring build after the session and 300 s idle; flat over the idle time. A few seconds after a start with nothing opened: 133 to 135 MB (first: 244) |
  | Idle memory, hidden to tray | ≤ 80 MB PSS | 197 MB (anonymous 120.1, file 76.6) | 132.8 / 129.9 / 132.4 MB (anonymous 60.1 to 62.7): MISS | the same, 300 s after the hidden window was released and the heap trimmed |
  | Idle CPU, shown and hidden | no periodic wakeups (the tool enforces ≤ 0.5 per second) | 0.03 and 0.02 wakeups/s | 0.04 and 0.01 wakeups/s in every run; busiest 100 s part 0.09; process CPU under one clock tick (10 ms) | voluntary context switches of all threads over 300 s; fake network, so no chat keepalive |
  | Receive → notification | ≤ 300 ms | 302 ms (worst 307) | 37 ms in run 3 (30 to 43); of that `recv.commit` → `notify.shown` 9 to 11 ms and the core's own part (`took_us`) 4.4 to 5.5 ms | fake network: the envelope handed to the backend's stage 1 → `Notify` accepted by the mock notification server; 5 starts with one live message each. Runs 1 and 2 read 38 ms (worst 92) and 88 ms: the tool's fresh 212 MB copy of the profile was still being written out, which made each commit's fsync take 22 to 35 ms instead of 5; the copy is synced before the app starts now |
  | Catch-up ingest, Signal mock | ≥ 500 messages/s | 10,479 (one run) | 9,766 / 9,435 / 9,219, slowest single run 8,812 | `kourier-e2e` `five_thousand_queued_messages_ingest_at_500_per_second` as a release build, 3 runs per full run: 5,000 queued 1:1 texts of one sender, connect → all committed |
  | Catch-up ingest, fake network, history with 8 % media | for information | 431 messages/s | 1,375 / 1,363 / 1,294 | the seeding of the reference profile, attachment downloads running beside it |
  | Timeline scroll, software renderer | for information (the 16 ms are for the GPU scene graph) | slowest frame 17.0 ms, median 15.9, 42 of 500 frames over 16.7 | slowest 17.7 / 18.1 / 18.6 ms, median 15.9 to 16.1, 62 / 21 / 24 frames over 16.7 | time between animation ticks while flicking 17,744 px through the image-heavy chat in 8 s |
  | Binary size | none | 48.3 MiB (247.9 MiB before the profile change) | 52.2 MiB (54,765,496 bytes; `.text` 27.6 MB) | `target/release/kourier`, default features |
  | The release build is non-dumpable | yes | yes | yes | the plain release binary runs and its `smaps_rollup` gives EACCES to the same user |

- Memory, what the 157 MB and 132 MB are made of. The file-backed part (70 MB) is at its upper
  bound: Kourier maps the current Qt library files, while 36 of the 38 Qt processes of the running
  desktop still map the files a package update replaced since the login, so no library page is
  shared (Qt6Gui: 6.5 MB resident, 0.0 shared). Beside three offscreen stand-in Kirigami programs
  on the same Qt files, the same fresh start has 97.1 MB PSS (anonymous 61.3, file 35.8) instead
  of 132.8 MB (61.2 + 71.5). The larger parts of a fresh start (132.8 MB): Qt6 libraries 34.5 MB,
  `[heap]` 28.3, unnamed anonymous memory 21.7 (11 MB of it in 15 glibc thread arenas), the binary
  16.7, other shared libraries 7.9, KDE Frameworks 7.5, QML plugins 4.5, the QML JS heap 3.0
  (first: 18.6), fonts and fontconfig caches 3.4; 209 shared objects are mapped (first: 309).
  Hidden, 20 s after the release, the anonymous 63.6 MB hold `[heap]` 25.5, 19 thread arenas 19.7
  (the largest 8.46 MB, the size of one connection's SQLite page cache), other unnamed memory 6.0
  and the JS heap 3.9. A heap profile of a start (massif, 47.5 MB shortly after the first frame)
  has no dominant site: SQLite `pcache1` 6.4 MB, `QImage` 4.8 MB (3.84 MB of it the offscreen
  backing store), 15.2 MB in 2,925 sites below 0.5 % each. `MALLOC_ARENA_MAX=2` or `=1` changes
  nothing (61.3, 61.3, 60.9 MB anonymous). A session with 21 opened chats and 8 s of flicking adds
  11 MB of heap in use and the release gives it back. The Signal backend is in none of these
  numbers.
- Not measured, because it needs the Plasma session, a Signal connection or the user (how to do
  each is in the open list): frame times and memory on Wayland with the GPU scene graph; PSS with
  shared library pages, after a login; a start after boot with the system's libraries cold; idle
  wake-ups and memory inside the app with a Signal connection; receive → notification on the
  Signal path and with plasmashell as the notification server; a catch-up against the real server;
  the §10.6 checklist; `PR_SET_DUMPABLE 0` with the portals; `install --user`; dogfooding.
- Where the first numbers came from, and what changed them:
  - Start (1,712 ms). The conversation list created a delegate for every one of the 199
    conversations at its first model reset and destroyed all but the visible ones right after:
    `MainWindow.qml` created the pages as `visible: false` children and pushed them only after
    `App.loaded`, so the rows arrived while the `ListView` was not in the page stack and had no
    bounded height. That cost 481 to 499 ms on the GUI thread, took the heap from 40 to 125 MB and
    left 65 MB freed but held by glibc (about 6 ms and 0.33 MB per conversation). The list now
    gets its model only once its page is in the stack and has a size
    (`ConversationListPage.rowsShown`): refresh 0 to 1 ms, heap at the first frame 40 MB in all 30
    starts, 18 delegates alive for 199 chats. `engine.load(Main.qml)` took 365 ms because
    `MainWindow` created every page at once (phase B had deferred this to S16): now only the
    conversation list is created with the window; the chat, welcome, link and settings pages and
    the emoji popup are created on first use, and in a wide window a cheap stand-in page holds the
    chat column for the first frame so the list does not jump.
  - Libraries. All 36 Qt image-format plugins and their codec libraries (libheif, libjxl, libavif,
    libraw, OpenEXR, Qt6Pdf) were loaded at start by one QML `Image` with a `qrc:` URL, the avatar
    badge: `QQuickPixmap::load` calls `QImageReader::supportedImageFormats`. It is a
    `Kirigami.Icon` now; only the SVG plugin is mapped at start (two on the media page).
    QtMultimedia with pipewire was loaded with the chat page, because `VoicePlayer.qml` imports it
    and `MessageDelegate.qml` named the type; the player is loaded by URL on first play.
  - Hidden memory (197 MB). The trim ran before the window's objects were freed: `destroy()` only
    schedules the deletion, so `malloc_trim(0)` saw 71 MB in use and the 34 MB freed two seconds
    later were never returned. The release is two steps now (§8.2 step 6); heap in use at the trim
    is 31 MB. A second trim runs 5 s after the first frame.
  - Receive → notification (302 ms). 250 ms of it was the desktop task's fixed coalescing delay.
    The first message of a conversation is now shown at once and only what follows within 250 ms
    is collected (`policy::Pace`).
  - Catch-up with media (431 messages/s, 53 s of CPU in a 129 s seed; `strace` of a 6,000-message
    seed: 3,656 fsyncs of 5.5 ms each). Most of it was the fake doing every download inline on its
    one actor thread, two blob fsyncs and a commit per picture: it now runs four downloads beside
    its command loop and commits their answers together. In the core, thumbnails are written
    without fsync and repaired after a start, and derive results are committed in groups of up to
    16. Not done: several envelopes in one `core.db` transaction (a forwarder has one envelope in
    flight and already ships up to 100 rows per envelope). The core's share of the path from stage
    1 to the notification event is one commit and was not changed.
  - Chat page (debug build, desktop style, Qt-thread CPU): creating it cost 195 ms, of which the
    message menu 37 ms, the reaction picker 60 ms and the who-reacted list 2 ms. The picker is
    created on first use now (132 ms). The menu stays eager: created on demand, its first opening
    was broken in about one run of three (CLAUDE.md trap).
- Release profile (root `Cargo.toml`): thin LTO and 4 codegen units stay, `debug = false`, `strip
  = "debuginfo"`. The binary was 260 MB, 210 MB of it DWARF: `debug = "line-tables-only"` still
  compiles the C and C++ parts (BoringSSL, SQLCipher, the cxx-qt glue) with full `-g`, because the
  cc crate knows only on and off. The symbol table stays (about 14 MB on disk, not loaded), so a
  panic backtrace names its functions. Measured against it: cargo's defaults (no LTO, 16 units)
  55.8 MB; fat LTO with one unit 38.9 MB, but a rebuild after touching one `kourier-app` source
  file takes 181 s instead of 61 s and peaks at 2.2 GB; `panic = "abort"` 42.7 MB, rejected
  because the supervisor restarts a panicking backend (`Exit::Panicked`), which needs unwinding.
  No variant differs measurably at run time on a fixture start, and none exports OpenSSL-named
  symbols. For the distribution build at the R1 gate fat LTO with one codegen unit is worth taking
  (smaller, and one unit helps reproducibility). The cold build time of the chosen profile was not
  measured cleanly (old profile 6 min 31 s, defaults 6 min 28 s, fat 7 min 53 s).
- §9.4 process hardening: release builds now really set `PR_SET_DUMPABLE 0`
  (`kourier_app::process::deny_dumps`, the first call in `main`). The section described it and the
  `dumpable` feature, and neither existed in the code. Every perf run checks it on the plain
  release binary. A build with `dumpable` prints `process::MEASURING_BUILD_NOTE` at start.
- Packaging and install (`packaging/`, `xtask/src/install.rs`). New
  `packaging/app.kourier.Kourier.metainfo.xml` (minimal AppStream metainfo; passes `appstreamcli
  validate --no-net`; the developer name "The Kourier
  developers" and the summary taken from the desktop file are placeholders for the R1 gate;
  there is no homepage entry while the project has no site).
  `cargo xtask install (--prefix <dir> | --user) [--no-build] [--scheme-handlers |
  --no-scheme-handlers] [--update-caches]` puts four files below the prefix (`--user` is
  `~/.local`): `bin/kourier` (0755), `share/applications/app.kourier.Kourier.desktop`,
  `share/icons/hicolor/scalable/apps/app.kourier.Kourier.svg` and
  `share/metainfo/app.kourier.Kourier.metainfo.xml` (0644). The installed desktop entry differs
  from the packaged one only in `Exec` and `TryExec`, which name the installed binary by its
  absolute path, quoted as the Desktop Entry Specification asks. Rules: the `MimeType` line
  (`sgnl://`, `signalcaptcha://`) is installed only when no other program's desktop entry claims
  one of the schemes (`--scheme-handlers` forces it, `--no-scheme-handlers` leaves it out) and the
  installer says which it did; with Signal Desktop present a captcha challenge therefore does not
  come back to Kourier by itself. A measuring build is refused (by its note's text). A
  `bin/kourier` that no Kourier desktop entry of the same prefix starts is not replaced, and
  `uninstall` leaves such a file alone (the name collides with other software). The desktop entry
  is written first and the binary last, through a temporary file that is created anew.
  `--update-caches` runs `update-desktop-database` and, where an icon cache exists,
  `gtk-update-icon-cache` on the prefix; never by default. `cargo xtask uninstall (--prefix <dir>
  | --user)` removes the four files and every directory that is empty afterwards, and prints each.
  The application id is one constant (`install::APP_ID`), and a test fails when `kourier-desktop`,
  `kourier-qt` or a packaging file disagrees. Install code ran only in temporary prefixes below
  `target/tmp`; nothing was installed into the real home or the system.
- §3.8, §4.2 contract additions (`kourier-model`, `kourier-backend-api`). `FileAlbum` gained
  `types` and `except` (MIME lists, skipped in JSON when empty so existing feature hashes do not
  change); the rule is `FileAlbum::takes(kind, mime)`: `(kinds has the kind and except lacks the
  type) or types has the type`, types compared without parameters and case, an empty type decided
  by kind. `ConversationFeatures::plan(files: &[PlanFile { kind, mime }], caption: Option<&Text>,
  reply) -> FilePlan`, with `FilePlan.caption_messages` (the texts of a caption that goes first,
  also one cut because it is above `max_text_bytes`; `message_count()` includes them, and
  `lead_text` is true for such a caption too); `plan_files(kinds, caption: bool, reply)` stays as
  a wrapper. `ConversationFeatures.disappearing_custom: BTreeMap<DisappearKind, TimerRange {
  min_secs, max_secs, step_secs }>` with `takes_timer(Duration)`, `has_timers()` and
  `TimerRange::contains`; `ConversationFeatures.hard_to_save: BTreeSet<String>` with
  `is_hard_to_save(file_name)` (both serde default and skipped when empty; features already stored
  in `core.db` lack them until the backend emits them again). `AttachmentKind::File` is documented
  as "never re-encoded, scaled or stripped by a backend". New trait method
  `Network::attachment_net_ids(local, index) -> Vec<String>` (default empty). New durable commands
  `Command::ClearConversation { cmd, conv, newest, newest_non_expiring, remove }` and
  `Command::DeleteAttachment { cmd, conv, target, index, net_ids }` (`Command::durable_id()`,
  `DurableCommand::{ClearConversation, DeleteAttachment}`), with `CLEAR_ANCHORS = 5`.
  `BackendEvent::Edited` gained `keeps_quote: Option<bool>` (serde default; every struct literal
  needs it). `BackendEvent::ConversationCleared` and `AttachmentDeleted` may be emitted before
  their messages, and a backend that knows a receipt's conversation sets `targets[i].conv`. MIME
  types come from `kourier_media::sniff_mime` on the first 64 bytes (the extension only as a
  fallback); every ISO-media brand that is not HEIC, HEIF, AVIF, M4A or QuickTime is reported as
  `video/mp4`, 3GP included. `kourier-blob` gained `BlobWriter::finish_volatile`,
  `BlobStore::import_volatile` and `sealed_len`.
- Facade (`kourier_core::facade`). `RequestId(u64)` is the UI's own id for one `SendFiles`; the
  core only echoes it. `UiCommand::SendFiles { conv, request, paths, as_file: Vec<bool>, caption,
  reply_to }` (missing `as_file` entries are false). `UiEvent::FilesStored { conv, request,
  messages }` after the commit and the row invalidations, `messages` in sending order (caption
  texts first); exactly one of `FilesStored` and `ActionFailed { action: "SendFiles" }` ends each
  request. `Progress::Import { conv, request, files_done, files, bytes_done, bytes }` inside
  `UiEvent::Progress`: never backwards, none for a send refused at the inspection. `RefusedFiles`
  gained `request` and `as_file`; `FileRefusal::Empty` is gone. `FilePlanQuery { conv, paths,
  as_file, caption, caption_text: Option<Text>, reply }`: with `caption_text` the plan counts a
  long caption exactly; `FilesPreview.kinds[i]` is what the file holds, also when it is sent as a
  file, so the composer knows which files have the choice. `CoreError::Refused { reason: Refusal,
  detail }` and `CoreError::refusal()`: `Refusal` is `TooOld`, `EditedTooOften`, `Deleted`,
  `BeingSent`, `Unchanged`, `NotAllowedHere`, `NotOwn`, `WrongKind`, `Empty`, `TooLong`,
  `TooMany`, `Blocked`, `CannotSend`, `Gone` (non-exhaustive); the log sentences are unchanged,
  and `InvalidInput` remains for requests that make no sense (no files, an attachment that is not
  downloaded, a bad reaction string, an empty challenge answer, Retry or Cancel with nothing to
  retry or cancel). `SetConversationTimer` accepts every value `takes_timer` accepts.
  `UiCommand::{ClearConversation { conv }, DeleteConversation { conv }, DeleteAttachment {
  attachment }}` with `MessageActions.delete_attachment` (the same check the command runs) and
  `ConversationRow.hidden` (true only on rows from `Query::Conversation(id)` for a deleted, empty
  chat). `UiCommand::ReleaseMemory` frees the page caches and prepared statements of all four
  connections. `Preview.file_name` names a lone document for the list.
  `kourier_core::testing::IngestHarness` gained `clear_conversation`, `delete_attachment` and
  `conversation`.
- §4.8 sends at scale (core; `outbox.rs`, `runtime.rs`, `attachments/import.rs`): the staged send,
  the compose lanes per conversation and the heads-only look are §4.8 steps 1 and 2. A
  user-visible consequence: compose commands keep their order per conversation only, and message-
  and attachment-keyed commands take one read to find their lane. Measured with 5,000 small
  documents in one `SendFiles` (release build, busy machine): the copy 16 to 59 s (99 s before the
  batched sync), stored in 79 transactions in 2.4 to 4.1 s, a write of another chat during the
  storing waited 30 to 45 ms median and 43 to 111 ms at worst, a text sent in another chat right
  behind the command went out 13 to 43 ms later, memory grew 16 MB until all was stored and 26 MB
  until all was sent. One look at an outbox with 5,000 waiting rows: 0.084 ms, was 8.05 ms (debug
  build). New failpoints `outbox_after_staged_commit` and `outbox_after_outcome_commit`.
- §4.8 challenges and waits (core; `outbox.rs`, `dispatch.rs`): the stored challenge, the interval
  a wait counts against a budget and the late attempt are §4.8 step 3. The same bound now ends the
  wait for a safety number. Rows behind a head the sweeper deleted start at once (the sweeper
  kicks the account's dispatcher through a control registry). After a restart an account can
  therefore show `SolveChallenge` without any send in this run.
- §3.5.2, §4.9 deletes made in Kourier and the ingest rules that changed (core; `ingest/clear.rs`,
  `modify.rs`, `local.rs`): `ClearConversation`, `DeleteConversation` and `DeleteAttachment`
  delete locally and store their durable command in one transaction; `DeleteConversation` also
  drops the draft, and when nothing a person sent is left the chat leaves the list
  (`ConversationsRemoved`) until its next message or until the user writes there.
  `DeleteAttachment` is refused with `Refusal::BeingSent` while the message's send row is in the
  outbox, because the request references the blobs (phase J: only while that row is queued,
  staged or in flight; a partly failed message takes the delete). The rules for what a clear takes, for a clear
  or an attachment delete that overtakes its messages, for receipts and for `keeps_quote` are in
  §4.9's table. A Retry after `AttachmentDeleted` sends the message as it is now
  (`prune_gone_files` in `take_due`).
- §4.3, §5.8 other core changes: `Command::Logout` follows `CancelLogin` and every login the core
  turns away. The long texts of a chat are fetched when it stops being blocked or request-pending.
  `download_orphans` and the thumbnail repair are §5.8's. Measured for the durable import (1,000
  files, three rounds taken in turns): 5.6, 12.0 and 8.4 ms per file as one batch against 11.9,
  38.4 and 24.1 ms with a sync after every file.
- §6.7 step 6 group receipts (Signal; `commands.rs`): as in §6.7. The `receipts` job's payload
  gained `group` (hex id; old jobs decode as 1:1) and `InGroup.named = false` keeps the group out
  of the sealed content. "Accepted" for a group is `groups.whitelisted` (the Storage Service
  record or the phone's request response, Desktop `markConversationRead` →
  `isConversationAccepted`); for an author without a 1:1 chat it is `peers.features_hash IS NULL`
  (Desktop's empty-chat case of `shouldSendToDirectConversation`). No group refresh is made for a
  receipt's token.
- §3.8 Signal's packing by format (`features.rs`; the module doc cites every source): the album is
  an allow-list of types with empty `kinds`, `ALBUM_TYPES` = `image/jpeg`, `image/png`,
  `image/gif`, `image/webp`, `image/bmp`, `video/mp4`: the intersection of what Desktop
  (`GoogleChrome.std.ts` `isImageTypeSupported` and `isVideoTypeSupported`; `ImageGrid.dom.tsx`:
  an album only when every attachment is visual), iOS (`CVComponentState.swift`
  `buildMediaAlbumItems`: one item that is not visual media discards the album;
  `MimeTypeUtil.swift` `isSupportedVisualMediaMimeType`) and Android
  (`MediaUtil.getSlideForAttachment`, `SlideDeck.getThumbnailSlides`, `VideoSlide.hasImage`) treat
  as visual by content type. So MP4 videos share an album with pictures, and HEIC, HEIF (Desktop
  cannot draw them), AVIF (not in iOS's list), QuickTime and Matroska (Desktop), WebM (iOS) and
  every type not listed travel alone, which is also the safe default for a format the sniffer
  learns later. Album size: `features::album_max` = min(`android.attachments.maxCount` − 1, 32),
  at least 1; the key is kept by name from the remote config, and `store_remote_config` now also
  compares the limits, so a changed value re-emits the features of every chat. The Android and iOS
  files were read from the public repositories, not from a checkout.
- §3.8, §6.7 timers and files as files (Signal): `disappearing_custom = { AfterRead: 1 s to
  2,419,200 s, step 1 }` for 1:1 chats, none for groups; `timer::change` enforces the upper bound
  (limits table: no enforcer was found, it is kept as unverified interop; phase J raised it to
  2,147,483,647 s, the bound Android enforces). An
  `AttachmentKind::File` with a picture's type goes out untouched under its name and its true
  content type (the protocol has no document marker); `send/media.rs` already keyed re-encoding on
  `Image`, the rule is now documented there and proven on the mock. `DESKTOP_WILL_NOT_SAVE` (47
  extensions of `SD/ts/util/isFileDangerous.std.ts`) is the `hard_to_save` of every Signal chat.
- §6.13 deletes for me from Kourier (Signal; new `delete_sync.rs`, job kind `delete_sync`): the
  mapping is §6.13's row. `Network::attachment_net_ids` is `uuid:<hex>` of the client UUID derived
  from the message id and the index, and the pointer takes its `clientUuid` from the name the core
  holds (`pointer_client_uuid`), so the phone can delete one picture of an album Kourier sent. A
  resent command finds its job or its done record and is confirmed every time.
- §6.13 identity merges (Signal; new `links.rs`, table `identity_links`): one funnel
  `links::learn` records a PNI → ACI pair and emits `IdentityMerged` once per pair, whichever
  source has it first: a Storage Service contact record with both ids, a verified PNI signature,
  and (new) a sealed sender's certificate that states for an ACI the number a contact record gives
  to a PNI without an ACI (Desktop's `maybeMergeContacts` on `EnvelopeUnsealed`). A rebuilt core
  gets every pair again (`links::all` in `rebuild_events`). This is what the core's receipt rule
  relies on: a receipt from a contact's ACI for a message in the PNI-keyed chat counts once the
  merge arrived.
- §6.12 challenges on jobs (Signal; `jobs.rs`, `challenge.rs`): as in §6.12. The standing
  challenge (announced once, newest token kept) is in memory; the marks `challenged` and
  `held_from` are in `backend.db`. (Phase J: the hold ends with the core's notice, also at a
  fresh `Online` and after a restart.)
- §6.7 step 2, §6.11 transfers (Signal; `attachment.rs`, `upload.rs`, `net/runner.rs`):
  `DownloadSink` is a struct that counts the bytes it took, the resume and the progress are
  §6.11's; `Net::https_upload_watched` runs the stall watch on the network runtime so the request
  really stops, and `Upload::body` returns the absolute goal before the producer starts, so an
  upload smaller than the queue is recognised as "all handed over". The mock CDN knew no ranges,
  so the harness serves `GET /cdn3/attachments/<key>` itself.
- Signal idle wake-ups, measured once by hand (`kourier-e2e` `tests/signal_idle.rs`, `#[ignore]`,
  100 s): a linked account on the mock with the real 30 s keepalive, 90 s idle, has 38 voluntary
  context switches over all 9 threads = 0.42 per second; the account actor thread 0.12, the
  network and core runtime workers 0.28. It is a test process, not the app.
- §8.3, §8.4 composer (UI): the waiting files are kept per chat for the session
  (`ComposerRust.parked`, `switch_to`, `strip_of`); a picture pasted from the clipboard is bound
  to the conversation that was open at the paste (it is encoded off the Qt thread), and the files
  of a refused send return to their own chat's list. Each chat's strip has its own range of entry
  ids and plan serials (`n << 32`), so a late answer never fits another chat's file. "Preparing"
  and the clean-up of pasted files follow the request id (`FilesStored`, `Progress::Import`); a
  resend after a partial refusal keeps each file's as-file choice. A picture or video row has a
  "send as original file" toggle (button and Space). A quiet note marks a type some client of the
  network will not save (`is_hard_to_save`); the file is still sent. The composer hands the plan a
  caption's text only above 500 characters (limits table).
- §8.3 timers, deleting, keyboard (UI): `dialogs/TimerDialog.qml` behind "Custom Time…" takes a
  number and a unit; the number field is `live` (an editable `SpinBox` otherwise applies typed
  text only when Enter is released or focus leaves) and bounded only by the range's end; "Set" is
  enabled only for a value of the stated range; `timer_label` spells mixed times ("1 minute 30
  seconds"). "Clear Chat History…" and "Delete Chat…" (`dialogs/ChatDeleteDialog.qml`) and "Delete
  Attachment…" (`dialogs/AttachmentDeleteDialog.qml`, which asks which one when the message has
  several) say in their confirmation what happens on the other devices (`deletes_sync`, from the
  same feature flag the core uses). Icons: the red Breeze icons (`edit-delete`) mark deletes that
  reach other people or remove a whole chat; deletes for this account only use `trash-empty`;
  clearing history uses `edit-clear-history`. Keyboard: Enter or Space on a row activates its
  picture, file or voice note only when the block names an action (`mediaAction`); a row without
  one, a sticker included, gets its menu and nothing else; Ctrl+Enter goes to the quoted message;
  each has a menu entry.
- §8.3, §8.6 other UI: the image viewer follows its message's row while the row is in the loaded
  window (a picture that is deleted or arrives), and has `movie = animated && !copy.notMovie`: a
  picture whose type claims animation but whose copy is not GIF or WebP by content shows as its
  still. The edit history refreshes while open. A lone document's name shows in the list preview.
  The drawer shows the account's number. A pinned chat with unread messages shows its pin beside
  the unread pill. The three looks items of phase H are fixed (the connecting line through a
  refused row, a fade at the file list's lower edge, a two-picture album as wide as its caption).
  Rows of hidden chats are filtered where rows enter the list. The answer to `Query::Accounts`
  sets `Changes.header` when a chat is open: the header's `cannot_send` and `deletes_sync` depend
  on the account's state, and the chat's own answers can arrive first (the composer stayed on
  "link this device again" after a relink).
- Bugs fixed that older phases had: a Qt message during process exit aborted the app with a core
  dump (the Qt log handler ran after thread-local storage was gone; `qtlog::end`); `--help` did
  not list every page (a test checks it now); the composer stuck after a relink (previous item);
  `crates/kourier`'s test `dev_control_flag` did not compile in a release test build (it is
  debug-only now). Test races fixed by their cause: `files_ui` and `core_ui` assert Qt-thread CPU
  time (`App.threadCpuMs()`) instead of wall time; `media_ui` waits for the first picture's
  thumbnail before it asks for a blurhash (the core derives both after the state is Local);
  `input_ui` waits for a popup's `opened` and for the view to rest after `positionViewAtEnd`;
  `actions_ui` sets a custom 12,345 s timer and is then refused five weeks. All QML tests force
  stderr logging.
- Viewmodel and bridge API: `UiSource::send_files` takes `as_file` and returns the request id;
  `RefusedSend.files` is `Vec<ReturnedFile>` with `request` and `resent_request`; `TimerSpan`
  (`contains`, `nearest`); `Failure::of` keeps `CoreError::Refused` in the refused class. QML:
  `MediaActions.view` and `viewAlbum` take the message id; `showList()` stays argument-free for
  dev-control and `showListWith(later)` is new; `App.threadCpuMs()`, `App.presence()` (0 nothing,
  1 focus, 2 chat on screen, 3 both) and `ConversationListPage.delegatesCreated` and
  `Main.releases` exist for tests; `App.findMessage` takes `editable`, `deletable`, `photo`,
  `album`, `removable`, `sticker`, `file`, `voice`, `quote`, `edited`, `reacted`, `own`;
  `TimelineView.pickerPopup` and `ensurePicker`. New QML files: `dialogs/{TimerDialog,
  ChatDeleteDialog, AttachmentDeleteDialog}.qml`, `composer/FileRowItem.qml`,
  `components/ChatEmptyState.qml`, `perf/PerfDriver.qml`.
- Fixture and screenshots: pages `originals` (the composer with the original-file choice and the
  type note), `timercustom`, `clearchat`, `mediamenu` (a row's menu with its media action and
  "Delete Attachment…") and `deleteattachment`; `PAGES` and `SHOT_PAGES` hold 34 pages and `cargo
  xtask shots` renders 136 PNGs. The fixture account has the number "+1 202 555 0143" (reserved
  for fiction); the albums chat ("Sol Vireo") starts with a sticker and its two-picture album has
  a long caption.
- §10.2 fake: `FakeNetwork::attachment_net_ids` returns `scripts::attachment_name(local, index)`;
  `scripts::signal_features()` states a custom timer range of 1 s to 4 weeks (`TIMER_MAX_SECS`;
  `SetConversationTimer` with 7 s or 45 s now succeeds on fake chats) and `hard_to_save: ["apk",
  "exe"]`; downloads run four at a time beside the command loop and their answers are committed in
  one transaction; new `Script::slow_downloads`; an actor finishes the files being written before
  it exits; a relink plays the rest of a script the account had not finished (kept in memory in
  `FakeControl`, so a relink in another process still skips it). For tests:
  `FakeControl::{durable_commands, script_progress, has_device}` and `kourier_fake::job_log(path,
  key) -> Vec<Job { cmd, kind, command }>` (the deletes made here have the kinds `clear` and
  `attachment`).
- §10.3 harness: `GET /cdn3/attachments/<key>` is served by the harness with ranges (200, 206 with
  `Content-Range`, 416, 404); new commands `breakDownloads` (resets a transfer part way, per
  request, optionally ignoring the range) and `downloadStats`; `setStorageState` contacts take
  `byPni` and `withPni`; `sendSyncSent` takes `destinationKind: "PNI"`. `smoke.mjs` has 21 cases.
- Dev hooks and demo: `KOURIER_TRACE_STARTUP=1` (any build); the perf flags of the binary (feature
  `perf` only): `--perf-profile=<dir>`, `--perf-size=<contacts>,<groups>,<messages per
  conversation>,<media percent>`, `--perf-seed`, `--perf-scenario=<name>` (`startup`, `gc`,
  `notify`, `session[:idle]`), `--perf-live-ms=<n>`; `KOURIER_DEV_RELEASE_MS` is read in perf
  builds too. Test knobs `SignalNetwork::{with_download_resume_pause, with_upload_stall}`;
  `KOURIER_SCALE_FILES` sets the size of the core's scale test. `cargo xtask demo` also fails
  unless both authors of a group get a read receipt for their messages (35 screenshots); its
  dev-control `page` command depends on `showList()` taking no argument.
- §10.2-§10.4 tests. Core: `tests/{budget, followups_i, scale, perf_marks, receipts,
  local_deletes}.rs`, more cases in `delete_for_me.rs`, `files.rs`, `revisions.rs` and
  `modifiers.rs` (the v7 → v8 rebuild), and in `outbox_crash.rs` a killed clear (4 kills), a
  killed large send (150 documents in three transactions, 6 kills: all or none, each message once)
  and a challenge wait across a kill; download orphans and account ids are covered by a clean
  restart, not a kill. Signal: unit tests for receipts, packing, deletes, links, the challenge
  hold, resume and stall; `kourier-e2e` `tests/signal_transfers.rs` and `signal_idle.rs`, new
  cases in `signal_{group_send, files, modifiers, sync_state}.rs`. UI: `kourier-qt`
  `tests/chat_ui.rs` (the real `MainWindow`, real core on the fake: lazy pages and bounded
  delegates at start, files per chat, original-file send checked in `core.db`, the keyboard
  actions, the dialogs) and `tests/release_ui.rs` (the real `Main.qml` shell: the two-step release
  and an activation between the steps; it skips in a release test build, where the release delay
  is a minute); `kourier-desktop` `a_bus_that_comes_back_is_listened_to_again`. xtask: 50 tests
  for the perf parsers, verdicts and table and for install and uninstall in scratch prefixes. Each
  lane ran its own suites on its own final sources (core and its libraries 414 tests;
  `kourier-signal` 264 unit and 53 mock tests, `kourier-e2e` 40, harness smoke 21;
  `kourier-viewmodel` 141 and the full `kourier-qt` suite), and each checked its new tests with
  mutation runs (a change that breaks the rule must make a test fail).
- New rows of the limits table (added to phase H's table, each marked "phase I"): the formats that
  share an album; the album size from the remote config; the custom timer's four-week bound; group
  receipts only for accepted groups and authors; the five anchors of a chat clear; two days for
  parked deletes for me and receipts; the 24 h age of a stored challenge; the download resume
  bounds; the upload stall times; one row of pacing constants; the composer's 500-character
  pre-filter. Rows changed in place: the album row, the empty file, the timer values, receipts,
  the send budget, the 45-day memory, the pasted file. Phase I adds no limit that refuses a user
  request except the timer's upper bound, which is to decide (phase J: four weeks is dropped
  under the limits rule; the bound that is left is the one Android enforces).
- Open for the next phase (what phase J closed is named in the last item; the first two items
  record decisions; everything else here is still open and is carried into phase J's list):
  - Decided by the user on 2026-10-07 and built in phase J. (1) Disappearing messages while the
    clock is unconfirmed (a start without a network) are a setting with three options: "hide
    now, erase once the time is confirmed" (the default), "keep until the time is confirmed"
    (what phase H built) and "erase by this computer's clock" (as Signal Desktop); §9.6. (2) A
    folder given to the composer (drop, paste or picker) asks each time: "add the files inside"
    (subfolders too, hidden files skipped, symbolic links not followed) or "send as one .zip";
    one question for several folders; the zip is built off the Qt thread and the size is
    checked against the chat's file limit before packing; §8.3.
  - Decided by the user on 2026-10-07: the proposed budgets of §1.3 are confirmed and are the
    budgets now. Anonymous memory (`Pss_Anon`) is enforced on every session and PSS against one
    limit for a session that shares its Qt files and another for one that does not; the idle
    number on the fake network is 0.2 wakeups per second over 5 min, with no 100 s part above
    0.5. `cargo xtask perf` enforces them (§1.3). Decided the same day: the installer keeps its
    default for `sgnl://` and `signalcaptcha://`. `cargo xtask install --user` registers Kourier
    for them only when no other desktop entry claims them (on this machine none does today, so
    it would register Kourier for both); `--scheme-handlers` and `--no-scheme-handlers` still
    force it either way.
  - Performance: phase I missed the two memory budgets the design had then (157 MB against 150
    shown, 130 to 133 MB against 80 hidden); the budgets were restated since (previous item).
    Still to do: re-measure after a login, when the desktop maps the same Qt files
    (`cargo xtask perf --no-mock`; expected about 122 and 98 MB), and consider a smaller page
    cache for the UI's read connections while hidden. The start has little headroom (366 ms as
    the worst run's median and 377 ms as the slowest start, against 400), and on the GPU the
    first frame also pays for the graphics
    context. Still loaded at start, worth 2 to 3 MB: Sonnet with its aspell, hunspell, voikko and
    hspell back ends (9 objects, 1.3 MB; a text field of the desktop style), the Kirigami Addons
    FormCard plugin (0.9 MB; it should come only with the settings page) and 13 KIO and widgets
    libraries that the KDE platform theme plugin brings (3.2 MB). The message menu is still
    created with the chat page (37 ms of its 132 ms of Qt-thread CPU in a debug build), so the
    first open of a chat pays for it inside `timeline.open`. Not re-measured on a quiet machine:
    the 5,000-file `SendFiles` numbers. Not measured at all: sending thousands of queued messages
    (commit-bound on the fake, 28 to 93 messages/s on a busy disk); the thumbnail check at start
    on a profile with tens of thousands of thumbnails (one `stat` each, in the background); a chat
    clear of tens of thousands of messages (one transaction, one `expired_messages` row per
    message). When about 200 MB of files had just been created on the same ext4 file system, each
    commit's fsync took 22 to 35 ms instead of 5: a restore or a burst of downloads next to a live
    message will show the same; it is inside every budget.
  - The perf tool: a release measuring build accepts only the production servers
    (`KOURIER_SIGNAL_ENV` is debug-only), so receive → notification on the Signal path, the Signal
    backend's memory and the keepalive's wake-ups inside the app are unmeasured; letting the
    feature `perf` accept `local:<handshake.json>` would let the tool link through the harness and
    time `recv.stored` → `recv.commit` → `notify.shown`. `recv.catchup` is never printed in a perf
    run (the fake never reports `Syncing`), and the e2e catch-up test prints its own line instead.
    The doc comment of `kourier_core::PERF_TARGET` (`supervisor.rs`) says a measuring build stamps
    each event through `kourier_app::trace`: no such subscriber exists, the tool reads the log.
    The catch-up test queues 1:1 texts of one sender and times connect → all committed, a superset
    of "stage 1 to core commit" but without the group texts §1.3 names: add sender-key group texts
    from several senders or reword the row. The reference profile is seeded again on every run
    with a build (40 s). The cold build time of the release profile is unmeasured, and
    `target/release` holds about 18 GB of profile experiments that can be deleted when no cargo
    runs.
  - Packaging and install: `cargo xtask install --user` has never run on a real home (the user's
    step; a Kourier desktop entry in `~/.local` can take `sgnl://` links). `PR_SET_DUMPABLE 0` is
    new in release builds and untested with the KDE file dialog, drag and drop from Dolphin, image
    paste and anything through `xdg-desktop-portal`; if something breaks, the switch is one call
    in `crates/kourier/src/main.rs`. `uninstall` also removes a directory that was empty before
    the install (`~/.local/bin`, for example); recording what an install created would need a
    fifth file in the prefix. The metainfo's homepage, developer name and summary are
    placeholders, and it has no releases yet (R1 gate, §14). The distribution build should take
    fat LTO with one codegen unit.
  - Contract: names an attachment gets only after upload or download (digest, plaintext hash)
    cannot be added to it later, there is no event for it, so a phone's delete that carries only
    `fallbackPlaintextHash`, or only a digest for an attachment Kourier sent, does not match.
    `outbox::retry` and `cancel` still answer `InvalidInput` for "nothing to retry", "not queued"
    and "reached some recipients already" (only `NotOwn` and `BeingSent` are typed there).
    `kourier-media` reports every unknown ISO-media brand (3GP too) as `video/mp4`, so such files
    join Signal albums. `plan_files`, the wrapper without MIME types, plans every file alone on a
    Signal chat: callers use `plan` or `Query::FilePlan`. Features stored in `core.db` lack
    `hard_to_save`, `disappearing_custom` and the album's types until the backend emits them
    again. An addition one lane made in another's crate, to be confirmed by the owner:
    `Preview.file_name` (core). Still
    open from phase G: `OutgoingKind::Edit` without `reply_to` and expiry (an edit made in Kourier
    of a reply whose quote is not in the backend's `sent_quotes`, because the reply is older than
    14 days or was sent before this device was linked, goes out without the quote; Desktop
    receivers then drop the quote and Kourier keeps showing it); one `admin_delete` flag and one
    window for normal and admin deletes; `QuoteSnapshot.thumb` outgoing only.
  - Core: the parked clear is an approximation in one case: when no anchor was present,
    messages that arrived before the clear event but after the phone's clear go with the first
    late anchor; when an anchor was present, older messages that arrive late survive. A logout
    deletes `pending_commands`, so a clear or attachment delete the backend had not confirmed is
    never told to the other devices. A send in parts: its first messages can show as queued
    while the rest is stored and vanish if a later part fails; after a merge during it, the
    later messages are composed in the old conversation's lane, so a text typed in the merged
    chat can leave before the rest of the files. After more than a day offline, the messages
    that were in flight when a challenge arrived (at most 4, one per conversation) need a manual
    Retry once the captcha is solved. The safety-number pause lives in memory only. Still open
    from phase H: a Failed row that gave up waiting for a challenge counts as "may be with the
    server" when it had an attempt; a message whose send gave up is usually outside the edit and
    delete windows; `ConfirmIdentity` during a link is not remembered; a long text over 256 KiB or
    empty stays a file; `AfterRead` timers started under a wrong clock; a held account that is
    logged out stays held; no test for the resend of pending commands at link time; the drain-path
    relink test relies on 300 ms of real time.
  - Signal: downloads resume only within one `DownloadAttachment` command and nothing partial
    is kept; downloaded files are still written with two fsyncs each (`finish_volatile` exists,
    but the core has no repair path for a lost download, so do not switch without one); the 60 s
    idle timeout of downloads is untested on the mock. Upload progress is counted where the body
    is produced, so a dead connection during the last 576 KiB, and for any smaller upload, is
    noticed after five minutes, not one; the stall watch is tested only against the scripted CDN
    (the harness cannot stall an upload). Group receipts: a group that is accepted only because we
    wrote there gets none until the phone's record arrives; an author whose 1:1 chat exists in the
    core without being accepted gets none even when that chat is empty; after a `core.db` rebuild
    `peers.features_hash` is NULL, so until a chat's features are sent again an author whose 1:1
    chat is an unanswered request counts as "no chat" and would get a group read receipt.
    Challenge hold: jobs are held only after a job met the 428 (a
    challenge met by a message send holds none, so the first job still makes one refused request
    and one notice); held jobs are
    not checked against `expires_at`. Jobs are at least once: a crash between the server's answer
    and the done record repeats a receipt or a `deleteForMe`. Identity merges by the certificate's
    number share Desktop's weakness (a contact record with a stale number joins the PNI chat to
    whoever holds that number now) and fire only when the sender shares the number. Seen once
    under load and not again: `kourier-e2e` `signal_groups` timed out waiting for the login's
    `Done`. Still open from phase H: a 428 on the sender-key send cannot be reported as a
    challenge with libsignal-net v0.104.0, so if a server challenged only that route the message
    would go out 1:1 and no challenge would show (check again at the next libsignal bump, §6.15);
    `preferContactAvatars` (after a change the other picture shows only once the re-fetch arrives,
    both pictures are not kept, a photo removed from the phone's address book is not handled); not
    verified from a source: the server's bound on the encrypted device name and whether iOS cuts a
    message's attachments on receive; Kourier's edit receive window is 48 h, Android's the delete
    window plus a day; a 0-byte attachment is untested on the mock's upload path. Still open from
    phase G: a group's timer cannot be changed (group writes); the `timer_update` job drops the
    peer send on a permanent error; contact-sync timers only from the initial sync; incoming call
    offers are dropped; group story replies and story reactions as `story-reply` placeholders (to
    decide); stage-1 backpressure does not serve the keepalive, hints or the unlink signal; the
    unlink drain is not cancel-aware and ignores commands; a Retry-After given at connect is cut
    short by a hint; a clock so wrong that TLS fails shows Offline, not `FixClock`; an unlink
    while offline or held is noticed only at the next connect; the PNI identity check of
    obligation 13 and a test for the group credentials
    refetch after a number change; 1:1 sends over raw REST; the interactive demo has no react,
    edit or delete commands and its `photo` command and `inbox` attachment lines were never run.
    Still open from phase F: the incremental MAC is not verified on receive, sticker send, avatars
    written on the account thread, the pointer cache (not keyed by plaintext hash, keeps pointers
    of deleted messages), an unknown CDN number and recipients reported unregistered (both to
    decide), follow-up transcripts list only the members of that attempt, no endorsement cache
    across sends, group send tokens not `Zeroizing`, the view-once flow.
  - UI: the perf items above (the eager message menu, Sonnet and FormCard at start). Files
    waiting in the composer are kept for the session only, and the conversation list gives no
    sign that a chat has files waiting; files waiting for a chat whose chat or account is
    removed on another device stay in memory (pasted ones on disk until exit); a send refused
    as a whole for a chat that is not open returns its files and caption but drops the quote.
    The viewer follows its message only while the row is in the loaded timeline window, and
    keeps what it shows if the chat is closed under it. The cause of a rare lost keyboard
    focus in `chat_ui` after a chat was opened is not identified (the test now checks focus and
    current row in the same turn; whether a user can hit it is unknown), and the cause found for
    the `input_ui` failure is probable, not proven. Untested: voice playback, opening a file in
    another application, the edit history's live update through the dialog (viewmodel only), the
    paste wiring for a chat that is not open; `media_ui` still instantiates `VoicePlayer`, which
    plays the fixture note if an audio device is reachable. Still open from phase H: the viewer
    shows "Play" for a video, which opens in another application; the edit history lists caption
    versions only for a message with files; the `files` fixture page waits only for the plan, so
    on a loaded machine its screenshot can show attachments at "0% downloading". Still open from
    phase G: an edit with two separate changes is one stretch, and the composer cannot create or
    show formatting or mentions; the emoji grid has no search, skin tones or recents; the jump to
    an original that is not loaded is tested in the viewmodel only; no Orca pass. Still open from
    phase F: no in-app video player, voice notes through Qt Multimedia's content probing, a media
    copy left in tmpfs after a lagged channel, an avatar replaced in place under the same key,
    real audio output untested.
  - Fake and harness: a relink in another process than the one that ran the script still skips the
    whole script. Still open: `smoke.mjs` cannot observe `sendSync` on a scripted device; the mock
    copies the client timestamp into `serverTimestamp` in its own senders and hands a freed device
    id out again.
  - Real server and manual, for the user. The real-server smoke run needs a dedicated test account
    and the user's OK in that session. From phases F and G: group send, attachments, upload resume
    on TUS and CDN 2, upload hosts under Signal's CA only, 401 or 403 for a removed device and the
    4401 close, whether the server keeps one-time keys on an upload without them, the real 428
    body with `options` and Retry-After, the answer to a wrong captcha and the captcha page flow,
    a silently dead TCP connection. From phase H: what the three clients show for an album of 31,
    a lone video and an album behind a caption over 2 KiB; a 0-byte file; a picture at level
    three; the Retry-After of a real 428 on sends and on jobs. New from phase I: an album of JPEG
    and MP4 on Desktop, Android and iOS; HEIC, HEIF and AVIF alone; a JPEG sent as a file (shown
    as a picture, saved as the original); a group read receipt; a timer of 95 seconds and one
    above four weeks; a `deleteForMe` for a cleared chat with `isFullDelete` false (Signal Desktop
    always sends true) and for a chat named by its PNI; service ids in binary form (Desktop sends
    them only under the remote flag `desktop.binaryServiceId`); a real CDN's answer to a range
    request; a catch-up after a day offline (rate limits, CDN downloads). Manual Plasma checks
    from earlier phases: drag and drop and paste of files from Dolphin, `Qt.openUrlExternally`,
    the "Open this file?" dialog, the real logind and NetworkManager signals (suspend and resume,
    a network switch), the browser handing a `signalcaptcha` link to Kourier (needs the desktop
    file installed and the MIME database updated), the §10.6 checklist. New on the Plasma session:
    frame times with `QT_LOGGING_RULES="qt.scenegraph.time.renderloop.debug=true"` and
    `KOURIER_LOG=qt=debug` while flicking through an image-heavy chat (no frame over 16 ms);
    memory of `cargo build --release -p kourier --features dumpable` from
    `/proc/<pid>/smaps_rollup` after 5 min shown and 5 min after closing to the tray (the 60 s
    release delay first; Mesa and driver mappings come on top, the 3.84 MB software backing store
    goes away; never with a real account); a start with cold system libraries (`sync; echo 3 |
    sudo tee /proc/sys/vm/drop_caches`, then `KOURIER_TRACE_STARTUP=1 kourier`); idle CPU with the
    Signal backend online (`powertop`, or `perf stat -e sched:sched_switch -p <pid>` over 10 min,
    which needs root here: `kernel.perf_event_paranoid` is 4); `cargo xtask install --user`, then
    Kourier in the launcher with its icon, `xdg-mime query default x-scheme-handler/sgnl`, and
    `cargo xtask uninstall --user` (`--update-caches` on a non-KDE desktop). A week of dogfooding
    with a dedicated test account, cross-checked against the phone.
  - Still open from phase E: bounded test parallelism for the mock suites (run by hand with
    `--test-threads 2`).
  - (Closed in phase J and removed from this list: `cargo xtask ci` and the demo ran on the
    merged phase I tree before its commits (1,101 tests, 136 screenshots; the project notes
    record it). The user's two decisions are built (first item). The four-week bound of the
    custom timer is decided by the limits rule. Performance: `UiCommand::ReleaseMemory` is sent
    when the hidden window is released and its effect is measured. Contract: the owner's check
    of `DESKTOP_WILL_NOT_SAVE`, of `is_hard_to_save` and of the `keeps_quote` line. Core: a
    parked receipt is tried again when its sender becomes a recipient by a membership update,
    by another device's send state or by the transmit's answer; `DeleteAttachment` for a partly
    failed message; a dropped staged send leaves a hidden chat hidden; an answered challenge is
    forgotten by the write behind the answer. Signal: `Logout` at the QR step without a
    warning; a link whose request is in flight, or that finished without a socket, no longer
    leaves its device on the server; `Logout` without a socket tells the server; the hold on
    jobs after a restart or a reconnect; the wire form of a group read receipt asserted on the
    mock. UI: files waiting for a merged chat follow the merge and two lists join; the custom
    timer dialog names the nearest times for an off-step value; the type note has a tooltip
    where it is cut; the hint watcher waits for a system bus that was absent at start.)

Phase J (the two user decisions of 2026-10-07: a setting for disappearing messages while the
clock is unconfirmed, and folders in the composer; the memory release with the hidden window and
its measurement; leftovers of phase I's open list in core, Signal and UI; 2026-10-07):

- Schema: `core.db` is version 9. `0009_expiry_hidden` (a plain step) adds
  `messages.expiry_hidden INTEGER NOT NULL DEFAULT 0` and the partial index
  `messages_expiry_hidden(account_id) WHERE expiry_hidden = 1`. It is a stored column and not a
  condition computed from the clock at query time: it holds across a kill, and the
  conversation counters are recounted in the transaction that flips it. The option needs no
  migration: it is the `settings` row scope `core`, key `expiry_policy`. Signal `backend.db`
  stays at version 12 and the fake's schema is unchanged.
- §9.6 the expiry option (core; `expiry.rs`, `runtime.rs`, `supervisor.rs`; user decision of
  2026-10-07). The hold phase H built is now one of three options, `ExpiryPolicy`, with the
  stable names `hide`, `keep` and `erase`:
  - `HideUntilConfirmed`, the default ("hide now, erase once the time is confirmed"): a
    message of an unconfirmed account that is due by the local clock is hidden at once and
    erased once a backend confirmed the clock; when the confirmed time says it is not due, it
    shows again as it was.
  - `KeepUntilConfirmed` ("keep until the time is confirmed"): phase H's behaviour. Nothing is
    hidden and nothing is erased until the clock is confirmed.
  - `EraseByLocalClock` ("erase by this computer's clock"): due messages are erased at once
    under any verdict. This is Signal Desktop's behaviour: `getExpiredMessages` compares
    `expiresAt` with `Date.now()` and asks no server (`SD/ts/sql/Server.node.ts:6199`,
    `SD/ts/services/expiringMessagesDeletion.preload.ts`).

  It is one setting for the whole core, applied per account by each account's own backend
  verdict: with two accounts at one start, the one that is online erases and the one without
  a network hides. No row and a value this build does not know both read as `hide`, the
  option that erases nothing early. None of the three is a limit copied from a reference app,
  so the limits table has no row for them.
- §9.6 in the sweeper (`expiry::Hold`, `look`, `settle`, `flip`, `sweep`). One look works from
  `Hold { policy, unconfirmed }`, read inside the sweeper's own write (`Shared::expiry_hold`:
  the stored `clock_wrong` set plus the in-memory `clock_unverified` set). Four row sets are
  spliced into its statements: `erasable` (due, account not held), `hideable` (due, still
  shown, account hides), `showable` (hidden, and not due any more or in an account that only
  keeps) and `awaited` (what the sweeper sleeps towards). `flip` sets or clears the flag,
  recounts each conversation (`ingest::recount`) and notes the conversation in
  `Changes.conversations` and `timelines`; hiding also puts every attachment of the rows into
  `media` and `media_gone`, which drops their media handles and removes open playback copies.
  Hiding and showing again leaves a database dump byte-identical. While anything is hidden,
  `look` asks for another look although no timer is pending, so the 60 s sleep cap
  (`MAX_SLEEP`) applies and a clock that was set back is noticed.
- §9.6 in the reads and commands. 17 conditions on `expiry_hidden` keep a hidden row out:
  the timeline, preview, quote, revisions, attachment and media-handle reads in `query.rs`
  (`MEDIA_SHOWN`), the counters in `ingest::recount` (unread, mentions, `last_activity`),
  `MarkRead` in `local.rs`, and the lookups of the commands in `outbox.rs`, `modify.rs`,
  `ingest/clear.rs` and `ingest/attachment.rs`, which answer `Refusal::Gone`. Each condition
  was removed once in a mutation run and a test failed each time. The desktop service needs no
  change: on `TimelineInvalidated` it reads the message it showed with
  `TimelineAnchor::Around(id)`, gets an empty page for a hidden one and closes the
  notification; on `ConversationsChanged` it closes at unread 0.
- §9.6 changing the option (`runtime::set_expiry_policy`, `expiry::set_policy`): the row is
  stored and the hide and show step runs in one transaction; a change to `erase` also hides
  there every message that is due and still shown (`DUE_AND_SHOWN`), in all accounts, and the
  sweeper, woken at once, erases them in its batches. All six directions between the three
  options are tested with a read right behind the command. A store that fails leaves the
  option and every message as they were and is reported as `UiEvent::ActionFailed { account:
  None, action: "SetExpiryPolicy", subject: ActionSubject::None, .. }` with an error whose
  `refusal()` is `None`. There is no event for a change.
- §9.6 at a start and at shutdown: `Core::start` reads the option (`expiry::stored_policy`),
  fills `clock_unverified` from the accounts whose backend it is about to start, and runs
  `expiry::settle_at_start` before any supervisor or the sweeper runs and before a handle
  exists. `Core::shutdown` sets `Shared::shutting_down`; `Shared::stopping()` makes the
  supervisors leave the two sets alone and the sweeper return.
- Three bugs found on the way, all in the core and all fixed with a test that fails without
  the fix. (1) A clean shutdown released the unverified-clock hold as each supervisor ended,
  and the sweeper could then delete held messages while the core was closing. Phase H's
  "keep" had this too: a quit while offline with due messages could erase them. (2) A change
  to `erase` that could not be stored still erased for good: the option in memory changed
  before the write and was taken back after the failure, and a sweep in between followed
  it. (3) The read that finds what to hide walked every message of each unconfirmed account.
  `core.db` has no planner statistics, and SQLite then prefers the equality on
  `messages_local(account_id, …)` to the range on `messages_expiring`; at a start every
  linked account is unconfirmed, so this ran at every start under the default option.
  Measured on plain SQLite 3.45 with 50,000 messages, warm: 12.5 ms against 0.013 ms. The
  statement now says `+account_id IN (…)`, and the unit test
  `expiry::tests::no_read_of_the_sweeper_walks_the_messages_of_an_account` runs `EXPLAIN QUERY
  PLAN` on the real engine for every option and every statement shape.
- §4.8, §4.9 core leftovers of phase I's list (`actions.rs`, `runtime.rs`, `outbox.rs`,
  `ingest/modifier.rs`, `ingest/roaming.rs`). `DeleteAttachment` is refused as `BeingSent`
  only while the message's send row is queued, staged or in flight (`actions::send_waits`: a
  `Failed` row does not count), so a message that reached some recipients and failed for the
  others takes it, as it takes the same delete made on the phone; "Retry" then sends the
  message without that file. It stays refused for a message nobody has yet. An answered
  challenge is forgotten by a write directly behind the backend's reply, before the pause is
  lifted (§4.8 step 3; new kill point `outbox_after_challenge_answer`). A staged send lists a
  hidden chat only in its last transaction (§4.8 step 1); the fix is on the storing side,
  because after a kill nothing records whether the chat was hidden before. A parked receipt is
  tried again at a `ConversationUpsert` that adds a member (`retry_receipts_in`), at
  `OwnSendState` and at the answer of the transmit (`retry_receipts_of`), §4.9.
- §8.3 folders in the composer (UI; user decision of 2026-10-07). The flow, the two answers
  and the walk's rules are §8.3's. How it is built:
  - `kourier_app::folders` (new, `crates/kourier-app/src/folders.rs`) does the disk work:
    `summarize(root, cancel, tick)` counts for the question, `list(root, cancel)` names the
    files for "Add the Files Inside", `pack(root, top, dest, cancel, progress)` writes the
    archive. All three block and run on the blocking pool (the composer controller's
    `spawn_folder_job` and `start_packing`); results are queued back to the Qt thread. One
    worker packs all the folders of an answer one after the other, because they read the same
    disk.
    Removing the row cancels its work; the app closing cancels counting and packing (the
    listing walk does not look at the shutdown token: it only reads and ends with the
    process).
  - The strip (`kourier_viewmodel::files`): a folder is an entry with a `FolderStage`
    (`counting`, `asking`, `listing`, `packing`), not a refused file. `Query::FilePlan` is
    asked about the entries that are not folders and its positions are mapped back, so the
    files around a folder can be sent meanwhile. The question is one `FolderQuestion` for all
    entries in the `asking` stage, and none is asked while any folder of that chat is still
    being counted. A folder that reaches the core before it was looked at (a send in the same
    turn as the drop) is refused there as not a file, comes back and is then asked about.
  - More of the walk than §8.3 says: a name that is not UTF-8 is walked and packed, written
    into the archive with replacement characters; `\` and control characters in a name become
    `_`; two names that come out alike get a number; an unreadable subfolder is counted, an
    unreadable given folder is an error; an archive that fails leaves no file behind.
  - The archive is written with the `zip` crate (8.6, MIT; default features off, feature
    `deflate-flate2-zlib-rs`, which uses the zlib-rs that flate2 already had in the lock; it
    brings `typed-path`), chosen over a hand-written ZIP64 writer. Entries are in walk order.
    The modification time is stored twice: the local time of the DOS field (to 2 s) and the
    exact UTC time in the Info-ZIP "extended timestamp" field. Of the mode only the permission
    bits are stored. ZIP64 (4 GiB and more, more than 65,535 entries) is the library's and was
    not exercised.
  - Where the archive lives and when it goes is §5.8: `AppHandle::reserve_archive(folder_name)`
    hands out a path in a new private directory; `AppHandle::release_pasted` and `is_pasted`
    cover archives too, and a release forgets the archive at once and deletes it on a blocking
    thread, so the call never waits for a disk.
  - The size check is `ArchiveRule { accepted, max }` from the open chat's
    `features.files[File]` (`max_size` 0 means the network states no limit for the kind). The
    .zip answer is not offered when the chat takes no plain files or one folder's files add
    up to more than `max`; the dialog then says why, with both sizes.
  - Two faults found in review and fixed. `pack` opened each file by its full path with
    `O_NOFOLLOW`, which guards only the last name: a subfolder swapped for a link between the
    walk and the packing led the read out of the folder. It now opens below the folder's
    descriptor (`open_at`, `open_below`; §9.4). And `ArchiveFiles::reserve` held the list's
    mutex while it made a directory, a mutex the UI thread takes: the two locks of §5.8.
  - The attach button opens a menu, "Files…" and "Folder…" (`AttachDialog` has a `folder`
    mode over QtQuick.Dialogs' `FolderDialog`). `attachmentCount` counts folder rows too.
    Filling the list with 1,001 files took 48 to 50 ms of Qt-thread CPU in a debug build.
- §8.2 step 6, §1.3 the memory release (UI; `App.windowReleasing`, `RELEASE_SETTLE`). The
  shell sends `UiCommand::ReleaseMemory` when it lets the hidden window go, and trims a second
  time 2 s after the first trim (before, that step only wrote the trace mark
  `window.released_settled`). Measured with `cargo xtask perf --quick --no-mock --report-only`,
  once by the lane and once on the final tree; a quick run idles 10 s where phase I's full
  runs idled 300 s, and both ran on a busy machine (load average 6.56 and 6.37), so start
  times are not comparable with phase I. The reports are generated files:
  `target/perf/report-phase-j-ui.md` and `report-phase-j-ui-review.md`.

  | | Phase I (three full runs) | Phase J, first run | Phase J, final tree |
  |---|---|---|---|
  | Hidden to tray, anonymous | 60.1 to 62.7 MB | 59.5 MB | 58.0 MB |
  | Hidden to tray, PSS | 129.9 to 132.8 MB | 109.6 MB | 108.2 MB |
  | Heap in use at the trim | 31 MB | 26 MB | 26 MB |
  | Shown, anonymous | 86.8 to 86.9 MB | 88.8 MB | 88.3 MB |
  | Shown, PSS | 156.5 to 157.8 MB | 138.9 MB | 138.5 MB |
  | File-backed part | 70 MB | 50.1 MB | 50.2 MB |
  | Start to first frame | 339 to 366 ms | 380 ms | 377 ms |
  | Open a conversation | 50 to 56 ms | 58.4 ms | 56.8 ms |
  | Receive → notification | 37 ms | 33.8 ms | 39.0 ms |
  | Binary size | 52.2 MiB | 52.5 MiB | 52.5 MiB |

  No budget of §1.3 is missed in either run. What the release itself brings is the heap in
  use, about 5 MB less at the trim, and between 0.6 and 4.7 MB of anonymous memory, not the
  10 to 15 MB phase I estimated from the arena sizes: after the trim 48 MB are still free
  inside the allocator's arenas, as fragments it cannot hand back. The other 20 MB the PSS
  fell by are file-backed and the session's doing: other processes carried 44 % of the Qt 6
  libraries (20.8 of 35.4 MB resident are mapped by others too), in phase I none. Shown, the
  anonymous memory read 1.4 to 2.0 MB higher than in phase I; a quick run cannot tell whether
  that is the shorter idle or this phase's code. The 60 MB hidden limit stays unenforced
  (§1.3).
- §8.9 the settings entry (UI; `settings/SettingsPage.qml`, `SettingsController`). The group
  "Disappearing Messages" has a lead line ("While the time cannot be confirmed") and three
  radio buttons, each with one sentence, reachable by keyboard and applied at once. QML sees
  `App.settings.expiryPolicy` (`hide`, `keep`, `erase`; empty until the core answered) and
  `App.settings.load()`. Behind it `UiSource::load_settings`, `expiry_policy()` and
  `set_expiry_policy(name)`, with `Changes.settings` and the query tracker `MODEL_SETTINGS`
  (7). `set_expiry_policy` sends the command first and shows the choice only when it left:
  stored before the send, the page kept an option a stopped core never got (found in
  review). An answer to a query sent before the command is about the old option and is
  dropped.
- Other UI (leftovers of phase I's list). Files waiting for a chat that is merged into another
  follow the merge, and two lists join (`UiSource::take_merges`, `Changes.merged`,
  `FileStrip::absorb`; open and parked chats). The hint watcher waits for a system bus that is
  absent at start (§4.7): `SystemHints::watch_reconnecting` returns `Self`, not a `Result`, and
  `connected_at_start()` lets the app say so once. The type note of a file row has a tooltip
  with the whole line where it is cut. The custom timer dialog says, for a time off the
  range's step, which times are nearest ("Times go in steps of … here: the nearest are … and
  …"; the dialog works them out itself, `TimerSpan::nearest` in the viewmodel is still called
  by tests only). Fixed on the way: the "Preparing N files" progress bar had no height in the
  desktop style and was never drawn.
- §4.3, §6.5 logout and a login that is stopped (Signal; `actor.rs`, `provisioning/link.rs`):
  as in §6.5's last paragraph. The cases of phase I's list, each tested on the mock: a
  `Logout` at the QR code (with and without a `CancelLogin` before it) exits `LoggedOut` in
  under 3 s with no warning, no account row and only the phone on the account; a login
  cancelled while its link request is held back 1.5 s (`delayLink`) removes the device the
  server linked, stores nothing and emits no `AccountReady`; a `Logout` right after the link,
  with the new device's first upgrade refused, removes the device on a socket of its own.
  Beyond the list: `Shutdown` in the link window takes the same path (the core discards a
  `Linking` account at its next start without a logout, `runtime::reconcile`, so an aborted
  login left a device on the phone); a `StartLogin` that arrives while a cancelled login is
  still ending brings that one to its end first (it was dropped as "login already running");
  a logout of a device the phone has removed already warns about nothing; a link whose
  account cannot be stored removes its device before it reports the failure. A cancelled
  login that had already stored its account keeps it only for the `Logout` that follows: no
  progress step, no connect. The price: removing an account while offline or held takes up
  to about 9 s (7 s for the connect attempt and the request, 2 s drain), inside the core's
  10 s.
- §3.8, §6.7 the timer range (Signal; `features.rs`, `timer.rs`; decided under the limits
  rule). `MAX_TIMER_SECS` is `i32::MAX`: a 1:1 chat states `disappearing_custom[AfterRead] = {
  min_secs: 1, max_secs: 2_147_483_647, step_secs: 1 }` (about 68 years; it was 2,419,200).
  The features hash changes, so `refresh_features` emits every chat's features again at the
  next start. `timer::change` refuses a value above the bound, below 1 s or with a sub-second
  part (`InvalidInput`); a timer another client states is taken as it comes
  (`timer::incoming`). Four weeks was only where the three official pickers end. What the
  clients do with a longer timer they receive, from their sources (Desktop from the local
  checkout, Android and iOS read file by file from the public repositories, main branch):
  - Desktop stores the number as it came (`SD/ts/textsecure/processDataMessage.preload.ts:677`,
    no check), words any length (`SD/ts/util/expirationTimer.std.ts` `format`), and its sweeper
    already waits in steps of at most 2^31 − 1 ms
    (`SD/ts/services/expiringMessagesDeletion.preload.ts:90`), which four weeks exceed.
  - Android: `SignalService.proto` `optional uint32 expireTimer`, read as a signed `Int`
    (`SignalServiceProtoUtil.kt:117` `(expireTimer ?: 0).seconds`), stored without a check
    (`DataMessageProcessor.kt:361` `inWholeSeconds.toInt()`), worded in weeks and days
    (`ExpirationUtil.java`, where `<= 0` is "off"). This is the bound that is kept: a value
    above 2^31 − 1 would arrive negative and show as "off". Picker:
    `CustomExpireTimerSelectorView.kt` `WEEKS(1, 4)`.
  - iOS stores a `UInt32` without a check
    (`DisappearingMessagesConfigurationRecord.swift:67-73`), computes the expiry in 64 bits
    (`TSMessage.m:498`) and words years, weeks and days (`String+SSK.swift:756`). Picker: weeks
    `1...4`; its comment puts a longer timer down to bugs and "poorly-behaved clients"
    (`DisappearingMessagesCustomTimePickerViewModel.init`), which is an opinion in a picker,
    not a check on receive.
  - The backup format has no upper bound (`LS/rust/message-backup/src/backup/chat.rs` checks
    only timers that are too short).

  The UI bridge carries the maximum as an `i32`, which this value fits exactly. The fake still
  states four weeks (`scripts::TIMER_MAX_SECS`), so the UI and core tests on the fake still
  see a five-week timer refused.
- §6.12 challenge holds (Signal; `actor.rs` `Shared::forget_job_challenge`, `challenge.rs`
  `Standing`, `jobs::release_challenged`): as in §6.12. The reason is the restart: the core
  showed its restored notice (§4.8 step 3) and the backend had neither the standing challenge
  nor a token, so the user's answer was refused with "nothing is waiting". Tested with a
  Retry-After of 600 s: one refused request and one notice per connect, not per job, and
  after a restart `SubmitChallenge { token: None }` is accepted and both delivery receipts
  go out.
- §6.13 the clear-chat flag (Signal; `delete_sync.rs`, its module doc has the citations):
  `isFullDelete: false` for "clear" is confirmed, nothing changed. Desktop
  (`SD/ts/util/deleteForMe.preload.ts:257`; `MessageReceiver.preload.ts:3682-3693` drops an
  entry without target messages), Android (`SyncMessageProcessor.kt`: `trimThread`, then the
  chat is deleted only `if (delete.isFullDelete == true)`; it sends false itself,
  `ThreadTable.kt:385, 416`) and iOS (`DeleteForMeSyncMessageReceiver.swift:114`, where
  `hasIsFullDelete` is required; `BulkDeleteInteractionJobQueue.swift`) agree. A unit test
  holds that false is encoded and survives a round trip.
- Lists and lines other lanes added to Signal's area in phase I, checked by their owner with
  no correction. `DESKTOP_WILL_NOT_SAVE` equals the 47 alternatives of
  `SD/ts/util/isFileDangerous.std.ts` in order (a unit test holds the list against Desktop's
  pattern string), and the model's `is_hard_to_save` follows Desktop's pattern. The facts
  behind `hard_to_save`: Desktop refuses to save such a file
  (`conversations.preload.ts:4192, 4263`), marks its thumbnail (`FileThumbnail.dom.tsx`) and
  refuses it in its own composer (`composer.preload.ts:1333`); Android saves any type
  (`SaveAttachmentUtil.kt` has no type test); no such list was found in iOS. `keeps_quote:
  Some(message.quote.is_some())` matches Desktop's `handleEditMessage.preload.ts:184-198` and
  Android's `EditMessageProcessor.kt:130-131`.
- §6.7 step 6 group read receipts on the wire (Signal; test only): the mock now proves what
  §6.7 says. A group read receipt is sealed, has the `Resendable` content hint, names no
  group in the sealed content and carries no PNI signature, also for an author who knows us
  only by number, while the same author's 1:1 receipt does carry the signature. Desktop sends
  the same (`SD/ts/textsecure/SendMessage.preload.ts:2540-2585`).
- §4.2 contract (`kourier-backend-api`): no new type or field. The doc comment of
  `BackendEvent::Receipts` names the retry points of a parked receipt. What the core sees
  from the Signal backend changed in two places: a login that got `Command::CancelLogin`
  emits no `Failed` step and can take until its link request is answered before it ends, and
  a `BackendNotice::ChallengeRequired` can come again at each fresh `Online` while the
  challenge stands, each time with a token that `SubmitChallenge { token: None }` answers.
- Facade (`kourier_core::facade`). `ExpiryPolicy { HideUntilConfirmed, KeepUntilConfirmed,
  EraseByLocalClock }` (also `kourier_core::ExpiryPolicy`; `Default` is the first) with `ALL`
  (the order a settings page lists them), `as_str()` (`hide`, `keep`, `erase`: what `core.db`
  stores, usable as a UI key, never translated) and `from_name(&str)`.
  `UiCommand::SetExpiryPolicy { policy }`, applied before anything sent after it is looked at.
  `Query::ExpiryPolicy` → `QueryData::ExpiryPolicy(ExpiryPolicy)`, answered from memory; a
  test double that matches `Query` exhaustively needs the arm (the one in
  `kourier-viewmodel/src/core_source/tests.rs` has it). Hidden messages need no UI rule:
  they are absent from `Query::Timeline`, `Preview`, `unread`, `mentions`, `ReplyPreview`
  (state `Missing`, `target` and `thumb` `None`), `Query::Revisions` and `Query::Attachment`
  (`None`) and the media handle table, and a stale command naming one gets `Refusal::Gone`.
  `MessageActions.delete_attachment` is true for `OutgoingState::PartiallyFailed`. New kill
  point `kourier_core::FAILPOINT_AFTER_CHALLENGE_ANSWER` (`outbox_after_challenge_answer`).
  `kourier_core::testing::IngestHarness` gained `sweep_unconfirmed(policy)`,
  `set_expiry_policy(policy, unconfirmed)` and `expiry_policy()`.
- `kourier-app` and `kourier-desktop` API. `kourier_app::folders`: `Cancel`, `Skipped {
  hidden, links, special, unreadable }`, `FolderSummary`, `FolderFile`, `FolderListing`,
  `Packed`, `FolderError` (no path or name in any variant), `summarize`, `list`, `pack`,
  `archive_name`, `ArchiveFiles { new, reserve, owns, take, release, clear }`, `remove_place`,
  `PROGRESS_EVERY`. `Paths.cache` and `Paths::archives_dir()`. `AppHandle::reserve_archive`.
  `FixtureOptions.expiring: Option<Duration>` with `EXPIRING_TEXT`: one read message in the
  showcase timer chat that comes due that long after delivery (the settings test).
  `kourier_desktop::hints`: `SystemHints::watch_reconnecting` returns `Self`,
  `connected_at_start()`, `ABSENT_MAX`.
- Viewmodel and bridge API. `files`: `StripFile.folder: Option<FolderEntry>`, `FolderStage`,
  `FolderCount`, `ListedFile`, `Counted`, `FolderJob`, `ArchiveRule`, `FolderQuestion`,
  `LARGE_ADD`; `FileFacts.folder`, `FileRow.stage` and `progress` (thousandths),
  `StripSummary.folders`; `FileStrip::{take_new_folders, contains, folder_counting,
  folder_counted, folder_question, answer_add, answer_zip, answer_cancel, folder_listed,
  folder_packing, folder_packed, absorb}` and the notices `folder_{added, empty, unreadable,
  unpacked, packed}_notice`. `UiSource::{archive_rule, take_merges, load_settings,
  expiry_policy, set_expiry_policy}`; `Changes.settings` and `Changes.merged`;
  `MODEL_SETTINGS = 7`. `refusal_text(NotAFile)` is "Not a file" and is not shown before the
  entry was looked at. QML: `App.composer.folderQuestion` (a map: `title`, `text`, `skipped`,
  `addLabel`, `addHint`, `zipLabel`, `zipHint`, `canZip`, `confirm`, `confirmLabel`,
  `confirming`), `folderAsking` and `answerFolders("add" | "zip" | "back" | "cancel")`; the
  file list roles `stage` and `progress`; `App.settings.expiryPolicy` and `load()`;
  `App.windowReleasing()`; `App.fixtureFolders(answer)`, which exists in release builds like
  the other fixture hooks and answers false outside fixture mode; `Composer.qml`
  `folderDialog`, `attachMenu` and `pick(folder)`; `SettingsPage.qml` `showExpiry()` and
  `expiryOptions`; `Main.qml` `coreReleases`. New QML file `dialogs/FolderQuestionDialog.qml`.
  Object names tests use: `attachButton`, `attachFiles`, `attachFolder`, `folderText`,
  `folderSkipped`, `folderAdd`, `folderZip`, `folderZipHint`, `folderWork`, `folderProgress`,
  `expiry-hide`, `expiry-keep`, `expiry-erase`.
- Limits table (phase H's table). Changed in place: the custom timer row (four weeks dropped,
  1 s to 2,147,483,647 s, enforcer Android's signed 32-bit read) and a note on the timer
  values row. New rows, each marked "phase J": the second question above 1,000 files of a
  folder; the .zip answer offered only up to the chat's file limit; what a folder's walk
  leaves out; the archive name cut at 200 bytes; one row of pacing constants. The core lane
  adds no limit. Phase J adds no limit that refuses a user request.
- Fixture and screenshots: pages `folder` (the question), `folderfiles` (a folder's files
  added to the list), `folderzip` (two folders as archives: `App.fixtureFolders` packs the
  first for real and holds the second at 45 % without a worker, so the picture does not
  depend on the disk) and `expiry` (the settings page at its "Disappearing Messages" group);
  `PAGES` and `SHOT_PAGES` hold 38 pages and `cargo xtask shots` renders 152 PNGs. The `files`
  page no longer has a folder, which would open the question: its unsendable entry is a file
  that is gone. Progress pictures of the four new pages, wide and narrow, light and dark:
  `target/shots-progress/j-ui-01` to `j-ui-16`.
- §10.2 fake: unchanged. Its timer range still ends at four weeks, on purpose its own value.
- §10.3 harness: new commands `failAuth { username: "<aci>.<deviceId>", status, count }` (for
  a device that is not linked yet; any 4xx or 5xx) and `delayLink { ms }` (holds the answer to
  `PUT /v1/devices/link` after the server linked); `waitForReceipt` also returns
  `sealedGroupId`, `contentHint` and `pniSignature`. Rust driver: `Harness::fail_auth_named`,
  `Harness::delay_link`, `ReceivedReceipt { sealed_group_id, content_hint, pni_signature }`.
  `kourier-e2e` support: `App::begin_link() -> (AccountId, String)` (adds the account, answers
  the device name, returns the first QR URL). `smoke.mjs` has 21 cases.
- Demo (`xtask/src/demo.rs`): before its first list screenshot it waits for all four chats,
  not three. The group is listed from the Storage Service sync before it has a message, and
  the server counts a message as delivered once the backend's stage 1 stored it, so three
  chats were there (two contacts and the empty group) while the third contact's 20 messages
  were still on their way, and the next step failed to open that chat on a loaded machine.
  Still 35 screenshots.
- §10.2-§10.4 tests. Core: `tests/expiry_policy.rs` (15 tests on the fake with the fake
  clock): each option at a start without a network, with a clock a day ahead and with a clock
  behind; a change in each of the six directions; a store that fails (a SQLite trigger
  refuses the settings row, 200 rounds); the start pass on a one-thread runtime that reads
  `core.db` before anything the start spawned has run; a message that comes due while others
  are hidden; a child process killed between hiding and erasing; three clean restarts (the
  test that catches the shutdown race); two accounts at one start; reading a chat; a hidden
  own message. `ClockJudge` (a small backend that gives a start a verdict on the clock, per
  account) moved to `tests/support/judge.rs`. 5 unit tests in `expiry.rs`. For the leftovers:
  `local_deletes::a_partly_failed_message_takes_the_delete_of_an_attachment`,
  `outbox_crash::an_answered_challenge_does_not_come_back_after_a_kill`,
  `receipts::a_waiting_receipt_counts_once_its_sender_becomes_a_recipient` and two unit tests
  in `outbox.rs`; the schema pins in `tests/modifiers.rs` say 9. UI: 13 unit tests in
  `kourier-app` `folders` (the walk's rules, the archive read back entry by entry, cancel, the
  store and its locks); `kourier-qt` `tests/folder_ui.rs` (the real `MainWindow`, real core
  on the fake: the menu, a folder dropped with a file, both answers down to the stored
  messages, the stored archive opened through `OpenMedia` and compared byte for byte, a
  150 MB folder not offered as an archive on the fake's 100 MiB limit, an empty folder, 1,001
  files asked about twice, the tooltip in a 380 px window) and `tests/settings_ui.rs` (a
  fixture account in the clock-skew state with one message due 3 s into the run: no query
  before the page is opened, then keep, hide and erase seen in the timeline); `release_ui`
  checks that each release asks the core once, before the trim; `kourier-desktop` `hints`
  covers a bus that is absent at start. `files_ui` now uses a socket and `actions_ui` a file
  that is gone where they used a folder. Signal: in `kourier-signal`
  `tests/robustness.rs` the logout, cancel and shutdown cases and
  `jobs_held_for_a_challenge_are_tried_again_after_a_reconnect_and_a_restart`, in
  `group_send.rs` the receipt's wire form, in `modifiers.rs` ten weeks and the maximum
  accepted and 2^31 s refused; in `kourier-e2e`
  `a_link_cancelled_at_its_last_step_leaves_no_device_on_the_phone` and
  `a_job_challenge_can_be_answered_after_a_restart`. Each lane ran its own suites on its own
  final sources (core and its libraries 425 tests; `kourier-signal` 266 unit and 61 mock
  tests, `kourier-e2e` 42 with 1 ignored, harness smoke 21; `kourier-viewmodel` 152,
  `kourier-app` 70 unit tests, the 26 unit tests and 13 test binaries of `kourier-qt`), with
  clippy, `deps-check` and qmllint clean, and each checked its new tests with mutation runs.
  `cargo xtask ci`, the full `cargo xtask shots` and a full `cargo xtask perf` were not run
  by any lane; the demo passed in the Signal lane while the UI lane was still working.
- Open for the next phase:
  - First: no run of `cargo xtask ci` or `cargo xtask demo` on the merged phase J tree is
    reported. Each lane ran its own suites (above); the Signal lane's demo ran before the UI
    lane had finished; the UI lane rendered the pages it touched, not the 152 PNGs of
    `cargo xtask shots`; both perf runs were quick ones, so no full `cargo xtask perf` has run
    against the confirmed budgets yet. The build steps of M1 and the two decided features are
    done on the mock; what follows needs the user.
  - To decide (user). (1) An account with no running backend (logged out, halted, or its
    network not in the build) is swept by the local clock under every option, as phase H built
    it: under "hide" and "keep" that erases without any confirmation. The likeliest case is an
    account whose first answer from the server at a start is "unlinked". The alternative keeps
    its due messages on disk, hidden or readable, until a relink. Until this is decided, the
    sentence under the first radio button promises more than such an account gets (it says the
    messages come back if the clock turns out to be wrong). (2) The option is one setting for
    all accounts, with a per-account effect; a choice per account would need another settings
    scope and an account parameter on the command and the query. (3) The .zip answer is
    offered only when the files add up to at most the chat's limit, as decided, so a folder
    that would compress to well below the limit is not offered; a "pack and see" path would
    close that. (4) The timer's four-week bound was dropped under the limits rule from the
    clients' sources, without a run on devices: confirm, or name another bound (one constant,
    `features::MAX_TIMER_SECS`). (5) Whether to tighten the hidden anonymous limit from 70 to
    60 MB (next item). Carried from earlier phases: group story replies and story reactions
    as `story-reply` placeholders; an unknown CDN number and recipients reported unregistered.
  - Performance: `ReleaseMemory` brings a few MB, not 10 to 15, so the 60 MB hidden limit
    would be met by 0.5 to 2 MB only, from two quick runs: do not tighten it (`MEMORY_HIDDEN`
    in `xtask/src/perf.rs` and §1.3) before a full run after a login
    (`cargo xtask perf --no-mock`; expected about 122 MB shown and 98 MB hidden PSS). Not
    known: whether the 1.4 to 2.0 MB more of anonymous memory with the window shown is the
    shorter idle or this phase's code; what the start pass of §9.6 costs at a start inside the
    app (its reads use the expiry index: 0.013 ms on plain SQLite with 50,000 messages; it
    adds one write transaction). The start has little headroom (366 ms as the worst full
    run's median in phase I, 377 and 380 ms in phase J's quick runs on a loaded machine,
    against 400), and on the GPU the first frame also pays for the graphics context. Still to
    consider: a smaller page cache for the UI's read connections while hidden. Still loaded at
    start, worth 2 to 3 MB: Sonnet with its back ends, the Kirigami Addons FormCard plugin
    (it should come only with the settings page) and 13 KIO and widgets libraries that the KDE
    platform theme plugin brings. The message menu is still created with the chat page. Not
    re-measured on a quiet machine: the 5,000-file `SendFiles` numbers. Not measured at all:
    sending thousands of queued messages; the thumbnail check at start on a profile with tens
    of thousands of thumbnails; a chat clear of tens of thousands of messages; a folder of
    tens of thousands of files added to the composer (the list is held in memory and filled
    in one model update on the Qt thread, about 50 ms per 1,000 files in a debug build; the
    second question guards it). A commit's fsync takes 22 to 35 ms instead of 5 right after
    about 200 MB of files were created on the same ext4 file system.
  - The perf tool (unchanged from phase I): a release measuring build accepts only the
    production servers, so receive → notification on the Signal path, the Signal backend's
    memory and the keepalive's wake-ups inside the app are unmeasured (letting the feature
    `perf` accept `local:<handshake.json>` would let the tool link through the harness);
    `recv.catchup` is never printed in a perf run; the doc comment of
    `kourier_core::PERF_TARGET` names a subscriber that does not exist; the catch-up test
    lacks the group texts §1.3 names; the reference profile is seeded again on every run with
    a build; the cold build time of the release profile is unmeasured. New: the tool builds
    into whatever `CARGO_TARGET_DIR` is set, so a lane's variable must be unset for it.
  - Packaging and install (unchanged from phase I): `cargo xtask install --user` has never run
    on a real home; `PR_SET_DUMPABLE 0` is untested with the KDE file dialog, drag and drop
    from Dolphin, image paste and anything through `xdg-desktop-portal` (the folder picker is
    one more such dialog); `uninstall` also removes a directory that was empty before the
    install; the metainfo's homepage, developer name and summary are placeholders and it has
    no releases (R1 gate, §14); the distribution build should take fat LTO with one codegen
    unit.
  - Contract. New: the viewmodel cannot see `AccountCaps::max_attachment_bytes`, so the
    archive's size check uses only the chat's limit for plain files (on Signal both come from
    the same remote value; a network with only an account limit would be offered an archive
    the core then refuses). There is no "challenge over" event: after a restart the core
    shows its restored job-challenge notice from the first `Online`; if the server's
    challenge has ended meanwhile, the held jobs go out, the backend has no token, the notice
    stays until its stored time or the next fresh `Online`, and an answer is refused with
    "Nothing is waiting for a verification". There is no event for a change of the expiry
    option (a second UI would have to ask). `ConversationFeatures::is_hard_to_save` lowercases
    and trims by Unicode rules where Desktop's pattern is ASCII-case with JavaScript's `\s`;
    they differ only for exotic characters (the Kelvin sign, U+0085, U+FEFF), where a
    composer note could differ. Carried from phase I: names an attachment gets only after
    upload or download (digest, plaintext hash) cannot be added to it later, so a phone's
    delete that carries only `fallbackPlaintextHash`, or only a digest for an attachment
    Kourier sent, does not match; `outbox::retry` and `cancel` still answer `InvalidInput` for
    "nothing to retry", "not queued" and "reached some recipients already"; `kourier-media`
    reports every unknown ISO-media brand (3GP too) as `video/mp4`, so such files join Signal
    albums; `plan_files` plans every file alone on a Signal chat (callers use `plan` or
    `Query::FilePlan`); features stored in `core.db` lack `hard_to_save`,
    `disappearing_custom` and the album's types until the backend emits them again;
    `Preview.file_name` (core) was added by another lane and is not confirmed by its owner.
    Carried from phase G: `OutgoingKind::Edit` without `reply_to` and expiry (an edit made in
    Kourier of a reply whose quote is not in the backend's `sent_quotes` goes out without the
    quote; Desktop receivers then drop the quote and Kourier keeps showing it); one
    `admin_delete` flag and one window for normal and admin deletes; `QuoteSnapshot.thumb`
    outgoing only.
  - Core, the expiry option: a clock that goes wrong after a backend confirmed it in the same
    run is not noticed, and due messages are then erased by the local clock under every
    option. An account that was held because its backend found the clock wrong and was then
    logged out stays held: under "hide" its due messages stay hidden and stored until a
    relink. A message that arrives already due into an unconfirmed account is stored as shown
    and may notify; the sweeper hides it at its wake-up, milliseconds later (a backend can
    deliver what a crash left in its `core_outbox` at a start without a network). When a
    hidden unread message shows again, its notification is not shown again; the row and the
    unread count come back. `OpenMedia` checks the row before it decrypts: a hide or an erase
    that commits during the decrypt of a large file leaves the plaintext copy in the tmpfs
    cache until `CloseMedia` or shutdown. A media handle registered by a timeline read that
    began before the hide can stay in the handle table until the message is erased (no UI row
    names it). A clock set back without any backend verdict shows hidden messages again
    within 60 s, not at once. No `kourier-app` test drives "hidden, so the notification
    closes" on the private bus (a step with `FixClock` in
    `crates/kourier-app/src/desktop/e2e.rs` would). Still open from phase G: `AfterRead`
    timers started under a wrong clock.
  - Core, other. `DeleteAttachment` is still refused for a message nobody has yet and for one
    that is queued or in flight. A kill between the backend's reply to a challenge answer and
    the write that forgets the challenge shows the solved challenge once more. An account the
    login stored whose `AccountReady` the core has not ingested when the app quits or crashes
    is discarded at the next start (`reconcile`) without a logout, so its device stays on the
    phone. Carried from phase I: the parked clear is an approximation in one case (when no
    anchor was present, messages that arrived before the clear event but after the phone's
    clear go with the first late anchor; when an anchor was present, older messages that
    arrive late survive); a logout deletes `pending_commands`, so a clear or attachment delete
    the backend had not confirmed is never told to the other devices; a send in parts can show
    its first messages as queued while the rest is stored, and after a merge during it the
    later messages are composed in the old conversation's lane; after more than a day offline,
    the messages that were in flight when a challenge arrived (at most 4) need a manual Retry
    once the captcha is solved; the safety-number pause lives in memory only. Carried from
    phase H: a Failed row that gave up waiting for a challenge counts as "may be with the
    server" when it had an attempt; a message whose send gave up is usually outside the edit
    and delete windows; `ConfirmIdentity` during a link is not remembered; a long text over
    256 KiB or empty stays a file; no test for the resend of pending commands at link time;
    the drain-path relink test relies on 300 ms of real time.
  - Signal, from phase J: a link request whose answer does not arrive in time still leaves a
    device nobody knows the id of (`Logout` and a `StartLogin` after a cancel wait 4 s,
    `Shutdown` 3 s, then the task is dropped with a warning to check the phone; a crash or a
    kill in that window does the same). A login that `CancelLogin` stopped after its account
    was stored keeps the account only for the `Logout` the core sends next; a `Shutdown`
    instead would leave the device. Removing an account while offline or held takes up to
    about 9 s, with little headroom inside the core's 10 s. While a challenge stands, every
    fresh `Online` costs one refused job request and one new notice, and a flapping
    connection repeats that. The timer evidence is from sources: for iOS the wire parse,
    storage, expiry arithmetic, both formatters and the picker were read, not every use of
    the duration; for Android the signed `Int` is inferred from the Kotlin code and Wire's
    `uint32` mapping, not from the generated class. That iOS has no list of file types it
    will not save rests on a search of file names. The fake's timer range and the custom
    timer dialog's units (below) were left as they are. The demo's second wait for three
    chats, after its restart, was left as it is: by then all four are in `core.db`.
  - Signal, carried from phase I: downloads resume only within one `DownloadAttachment`
    command and nothing partial is kept; downloaded files are written with two fsyncs each
    (the core has no repair path for a lost download); the 60 s idle timeout of downloads is
    untested on the mock; a dead connection during the last 576 KiB of an upload, and for any
    smaller upload, is noticed after five minutes, and the stall watch is tested only against
    the scripted CDN. Group receipts: a group that is accepted only because we wrote there
    gets none until the phone's record arrives; an author whose 1:1 chat exists in the core
    without being accepted gets none even when that chat is empty; after a `core.db` rebuild
    an author whose 1:1 chat is an unanswered request counts as "no chat" until the chat's
    features are sent again. Challenge hold: a challenge met by a message send holds no jobs,
    and held jobs are not checked against `expires_at`. Jobs are at least once: a crash
    between the server's answer and the done record repeats a receipt or a `deleteForMe`.
    Identity merges by the certificate's number share Desktop's weakness (a contact record
    with a stale number joins the PNI chat to whoever holds that number now) and fire only
    when the sender shares the number. Seen once under load: `kourier-e2e` `signal_groups`
    timed out waiting for the login's `Done`. Carried from phase H: a 428 on the sender-key
    send cannot be
    reported as a challenge with libsignal-net v0.104.0 (check again at the next bump,
    §6.15); `preferContactAvatars` (the other picture shows only once the re-fetch arrives,
    both pictures are not kept, a photo removed from the phone's address book is not
    handled); not verified from a source: the server's bound on the encrypted device name and
    whether iOS cuts a message's attachments on receive; Kourier's edit receive window is
    48 h, Android's the delete window plus a day; a 0-byte attachment is untested on the
    mock's upload path. Carried from phase G: a group's timer cannot be changed (group
    writes); the `timer_update` job drops the peer send on a permanent error; contact-sync
    timers only from the initial sync; incoming call offers are dropped; stage-1 backpressure
    does not serve the keepalive, hints or the unlink signal; the unlink drain is not
    cancel-aware and ignores commands; a Retry-After given at connect is cut short by a hint;
    a clock so wrong that TLS fails shows Offline, not `FixClock`; an unlink while offline or
    held is noticed only at the next connect; the PNI identity check of obligation 13 and a
    test for the group credentials refetch after a number change; 1:1 sends over raw REST; the
    interactive demo has no react, edit or delete commands and its `photo` command and `inbox`
    attachment lines were never run. Carried from phase F: the incremental MAC is not
    verified on receive, sticker send, avatars written on the account thread, the pointer
    cache (not keyed by plaintext hash, keeps pointers of deleted messages), follow-up
    transcripts list only the members of that attempt, no endorsement cache across sends,
    group send tokens not `Zeroizing`, the view-once flow.
  - UI, folders: "Add the Files Inside" hands paths to the core, which opens them at send
    time, so a file or a subfolder swapped for a link between the walk and the send is
    followed there (closing it needs the core to take descriptors or to check again); the
    walk itself checks an entry with `symlink_metadata` and then lists it by path, so a
    subfolder swapped for a link in that moment is listed (for an archive its files then fail
    to open and count as missing). An archive of data that does not compress, within about
    0.1 % of the chat's limit, comes out larger than the sum of the files, is marked "Too
    large" after packing and has to be removed and dropped again. There is no free-space
    check before packing (a full disk ends in a notice), and the archive is a plaintext copy
    in the cache directory until the send is stored: a folder from an encrypted volume gets a
    copy outside it when the cache is not on one, and a crash leaves it until the next start.
    The archive holds regular files only (no empty folders, no links as links); two names
    that differ only in case extract onto each other on a case-insensitive system. A folder
    sent in the same event-loop turn it was dropped is refused by the core, comes back and is
    then asked about, with a notice in between that a file could not be sent. The question
    for a chat that is not open waits until the chat is opened, and notices about folders of
    a chat that is not open are dropped. Two notices are built from sentence halves and will
    not translate well. Qt prints `DelegateModel::cancel: index out range` once when a list
    of 1,001 rows is cleared (harmless, not one of the lines `cargo xtask shots` fails on).
    Not tested through the UI: a merge with files waiting, and removing a row while its walk
    or packing really runs (both are tested below the UI).
  - UI, other: the custom timer dialog's largest unit is weeks, so a Signal chat's upper end
    reads as about 3,550 weeks; a months or years unit would read better. On a machine that
    never has a system bus the hint watcher makes one connect attempt every 10 minutes.
    Carried from phase I: the perf items above (the eager message menu, Sonnet and FormCard
    at start). Files waiting in the composer are kept for the session only, and the
    conversation list gives no sign that a chat has files waiting; files waiting for a chat
    whose chat or account is removed on another device stay in memory (pasted ones on disk
    until exit); a send refused as a whole for a chat that is not open returns its files and
    caption but drops the quote. The viewer follows its message only while the row is in the
    loaded timeline window, and keeps what it shows if the chat is closed under it. The cause
    of a rare lost keyboard focus in `chat_ui` after a chat was opened is not identified, and
    the cause found for the `input_ui` failure is probable, not proven. Untested: voice
    playback, opening a file in another application, the edit history's live update through
    the dialog, the paste wiring for a chat that is not open; `media_ui` still instantiates
    `VoicePlayer`, which plays the fixture note if an audio device is reachable. Carried from
    phase H: the viewer shows "Play" for a video, which opens in another application; the
    edit history lists caption versions only for a message with files; the `files` fixture
    page waits only for the plan, so on a loaded machine its screenshot can show attachments
    at "0% downloading". Carried from phase G: an edit with two separate changes is one
    stretch, and the composer cannot create or show formatting or mentions; the emoji grid
    has no search, skin tones or recents; the jump to an original that is not loaded is
    tested in the viewmodel only; no Orca pass. Carried from phase F: no in-app video player,
    voice notes through Qt Multimedia's content probing, a media copy left in tmpfs after a
    lagged channel, an avatar replaced in place under the same key, real audio output
    untested.
  - Fake and harness: the fake states a timer range of four weeks where Signal now states
    2,147,483,647 s, so no UI or core test sends a longer timer through the fake. Carried: a
    relink in another process than the one that ran the script still skips the whole script;
    `smoke.mjs` cannot observe `sendSync` on a scripted device; the mock copies the client
    timestamp into `serverTimestamp` in its own senders and hands a freed device id out
    again.
  - Real server and manual, for the user. The real-server smoke run needs a dedicated test
    account and the user's OK in that session. New from phase J: a timer above four weeks set
    in Kourier, as Android and iOS show and run it; whether the server minds one refused job
    request per connect while a challenge stands, or extends the challenge for it. New on the
    Plasma session: the "Folder…" picker (QtQuick.Dialogs `FolderDialog`, the portal), a
    folder dropped from Dolphin and a copied folder pasted. From phases F and G: group send,
    attachments, upload resume on TUS and CDN 2, upload hosts under Signal's CA only, 401 or
    403 for a removed device and the 4401 close,
    whether the server keeps one-time keys on an upload without them, the real 428 body with
    `options` and Retry-After, the answer to a wrong captcha and the captcha page flow, a
    silently dead TCP connection. From phase H: what the three clients show for an album of
    31, a lone video and an album behind a caption over 2 KiB; a 0-byte file; a picture at
    level three; the Retry-After of a real 428 on sends and on jobs. From phase I: an album of
    JPEG and MP4 on Desktop, Android and iOS; HEIC, HEIF and AVIF alone; a JPEG sent as a
    file; a group read receipt; a timer of 95 seconds; a `deleteForMe` for a cleared chat with
    `isFullDelete` false and for a chat named by its PNI; service ids in binary form; a real
    CDN's answer to a range request; a catch-up after a day offline. Manual Plasma checks from
    earlier phases: drag and drop and paste of files from Dolphin, `Qt.openUrlExternally`, the
    "Open this file?" dialog, the real logind and NetworkManager signals (suspend and resume,
    a network switch), the browser handing a `signalcaptcha` link to Kourier (needs the
    desktop file installed and the MIME database updated), the §10.6 checklist; frame times
    with `QT_LOGGING_RULES="qt.scenegraph.time.renderloop.debug=true"` and
    `KOURIER_LOG=qt=debug` while flicking through an image-heavy chat (no frame over 16 ms);
    memory of `cargo build --release -p kourier --features dumpable` from
    `/proc/<pid>/smaps_rollup` after 5 min shown and 5 min after closing to the tray (never
    with a real account); a start with cold system libraries (`sync; echo 3 | sudo tee
    /proc/sys/vm/drop_caches`, then `KOURIER_TRACE_STARTUP=1 kourier`); idle CPU with the
    Signal backend online (`powertop`, or `perf stat -e sched:sched_switch -p <pid>` over
    10 min, which needs root here); `cargo xtask install --user`, then Kourier in the launcher
    with its icon, `xdg-mime query default x-scheme-handler/sgnl`, and
    `cargo xtask uninstall --user`. A week of dogfooding with a dedicated test account,
    cross-checked against the phone.
  - Still open from phase E: bounded test parallelism for the mock suites (run by hand with
    `--test-threads 2`).

Phase K (before the first real-server run: the fixes an audit of the production path asked
for, and the smoke-run tool; 2026-10-07). Nothing in this phase talked to a real server: every
test runs on the mock or without a network. The runbook is `docs/smoke-run.md`.

- User requirement (2026-10-07): the login of the smoke profile is kept. It is linked once with
  a dedicated test account and used by every later run; the QR code is scanned by the test
  account only, and the peer is the user's personal account, to which nothing is ever linked.
- §6.5 step 6, §9.2: an account is bound to the server that linked it. `backend.db` schema 13
  adds `account.env` (`production`, `staging`, `local:<host>:<port>`; `net::env::binding`). The
  actor checks it before any job is queued (`net::env::check_binding`) and exits
  `Failed(Unsupported)` with both names; nothing connects and nothing is wiped. An account
  linked before the column existed (`NULL`) passes only for a local test server: no real server
  had been used then. Before this, a start of a production profile with a leftover
  `KOURIER_SIGNAL_ENV=local:…` got the mock's 403 for unknown credentials, which
  `connection::run` reads as "unlinked", and the core wiped the login.
- §4.3 step 4: the supervisor halts an account whose backend exits `Failed(Unsupported)` at
  once (`AccountError`, not restartable) instead of restarting it five times.
- §6.5 step 5: `attachmentBackfill` is `true`, as Desktop sends it. The other three flags
  already matched. Which capabilities the server takes from a linked device could not be read
  from any source available offline, so Kourier claims no less than Desktop. The test
  `capabilities_serialize_like_desktop` now compares all four keys with Desktop's values.
- §4.4, §6.5 step 3: `BackendConfig::expect_number_suffix` (debug builds:
  `KOURIER_DEV_EXPECT_NUMBER_TAIL`). The Signal link compares the provisioned number's last
  digits right after the provisioning message is decrypted (`provisioning::link::link`), before
  `PUT /v1/devices/link`. A mismatch fails the login with nothing registered. A value that is
  not 2 to 15 digits fails the start, so a typo never turns the check off.
- §8.8, §9.2: one process per profile. `ProfileLock` is an exclusive `flock` on `data/lock`,
  taken in `App::start` for a normal launch after the bus hand-over (a second desktop launch
  still forwards to the first) and held until the app is gone. `AppError::ProfileInUse`.
- §5.2: the development keyring is made only for a profile without data
  (`KeyError::DevKeyringMissing` otherwise). `KeyError::Unwrap` maps to the new
  `VaultError::WrongKey` instead of `SecretMissing`: the start-up reconciliation leaves such an
  account directory untouched and logs an error, and the supervisor halts a known account in
  that state without the logout wipe. Before this, a profile restored without its `config/` and
  `core.db` got a new key, and the start removed the account directory as "shredded".
- §9.1: `logging::redact` on every line to stderr and to the file; the request log target
  `kourier_signal::requests` (`net::runner`: route, status and timing of every request and
  connect, DEBUG, off by default).
- §9.4: `KOURIER_DEV_NO_DUMP=1` makes a debug build non-dumpable.
- Dev control (`crates/kourier-qt/src/dev`, debug only): `chats` (the list as ids, kinds and
  counts, no titles), `open_id {conversation}`, `state.timer_secs`, and `KOURIER_DEV_QR_URL=0`
  (the `login_qr` event then carries no link). A driver whose output others read never has to
  handle a name or the link.
- `cargo xtask smoke` (`xtask/src/smoke.rs`): `start`, `step <name>`, `steps`, `state`,
  `report`, `stop`, `backup`, `wipe`, `rehearse`. A detached supervisor process (a session of
  its own) owns the app's dev-control pipes and a Unix socket in the profile directory, so every
  command is a short shell call. The kept profile is
  `${XDG_DATA_HOME:-~/.local/share}/kourier-smoke/<name>` (0700), given to the app as
  `KOURIER_HOME` with the development keyring: the directory is the whole login and the
  desktop's keyring is never touched. That keyring's passphrase is a public constant, so at
  rest only the disk encryption (checked from sysfs at `start`; a plain disk is refused) and the
  mode protect it. The tool's own state file records the server, linked since, last online, the
  expected tail, the build and schema, and the sends used. `start` refuses a real server without
  `--confirmed-by-user`, another server than the profile's, a first link without
  `--expect-tail`, a second app on the profile and a changed schema without
  `--allow-schema-change`; it strips every inherited `KOURIER_*` variable and sets its own.
  A local environment gets a throwaway directory below the target directory. Desktop services
  are off by default (no notifications with names and texts on the desktop). The idle deadline
  is 30 days (Signal's support text; not in Desktop's sources), warned from day 21. Sends are
  counted by the supervisor before a sending command leaves (default cap 30). Steps that end the
  login need `--destroys-login`, `wipe` needs `--yes-delete-the-login`.
- Tests: `kourier-signal/tests/robustness.rs` (a start for another server is refused and the
  login survives; a first link by an unexpected account registers nothing), unit tests for the
  binding, the number check and the capabilities; `kourier-core/tests/lifecycle.rs` (an account
  directory the key does not open is left untouched; a backend that refuses its account is not
  restarted); `kourier-app` (keyring guard, wrong key, lock, redaction, no-dump switch, the
  expectation variable); `crates/kourier/tests/hardening.rs` (the real binary: non-dumpable and
  no core dump after SIGABRT, a second process refused); `dev_control.rs` (the name-free
  commands, the link kept in the process); 19 unit tests in `xtask`. `cargo xtask smoke
  rehearse` runs the whole tool against the mock with the harness as the phone: 19 steps pass
  (the group send with a sender key to two members; a message that arrives after the last
  restart), the refusals are checked (a second start, an unexpected account, every 1:1 sending
  step before the timer was compared, a wrong timer, the send cap, another server, the
  login-ending step and the wipe without their flags, the supervisor run by hand), a link step
  that gave up waiting is picked up by the next one, a refused link is followed by a new login
  in the same app, and everything the tool and the app wrote is searched for the mock world's
  names, numbers, ids, texts, the device name and the QR link.
- Review before the first run (same day; the reviewer's fixes, each with a test and a
  mutation that the test kills):
  - Dev control `state.timer_secs` is `null` until the open conversation's header is loaded
    (`TimelineModel::header_loaded`). It used to be 0 from the moment `open_id` returned to
    the moment the core's answer arrived, and 0 reads as "off": the tool's comparison of
    Kourier's timer with the phone's could pass with `--timer off` for a chat whose real timer
    was another, and the send would then have set the peer's chat to Kourier's value.
  - The tool compares the timer before every send to the peer, not only in `step send`:
    `attachment-send`, `react`, `reply`, `edit` and `delete` go through the same gate
    (`StepRun::peer_for_send`, `timer_to_send_with`). The value `step send --timer` confirmed
    is kept in the state file (`peer_timer_secs`; forgotten when the peer chat changes); a
    step sends only while Kourier shows that value, or with `--timer` of its own. An unknown
    timer is an error, never "off".
  - A sending step sends nothing while the open chat's composer holds files
    (`send_blocker`): a step that attached files and failed before its send left them there,
    and the next text would have taken them along (more messages than the step says).
  - The supervisor starts the app only on a launch order `start` wrote within the last two
    minutes, for the server of the profile's state file, and takes the order away when it
    reads it (`take_launch`). Before, `smoke/launch.json` stayed, and `cargo xtask smoke
    __supervise --home <profile>` run by hand started the app for production again without
    `--confirmed-by-user`; an order without a server meant production.
  - `--env local:…` refuses `--profile`: a mock run cannot make or use a profile in the kept
    profiles' directory (it used to, and a mock profile named `default` would then have stood
    in the way of the first real start, with a wipe of that path as the way out).
  - The state file learns "linked" again from an account that is online without a login in
    that run (the supervisor killed between the end of a login and its next save left a
    profile that `start` took for unlinked and `step link` refused).
  - The remembered chats (`peer_conv`, `group_conv`) are row numbers of `core.db`. They are
    kept with the file they were seen in (`chats_db`: device and inode) and forgotten at a
    `start` that finds another file there (a rebuilt or restored database numbers its chats
    anew, and a send would go to the wrong chat); a step also checks that a remembered chat
    is still a chat of its kind in the list (`still_the_chat`).
  - Every step prints the profile and the server it acts on in its first line; a start on a
    profile whose login has ended no longer says "next: step link"; the app's stderr is read
    to its end whatever the log file or a line's encoding does.
  - Seen once in six runs of `kourier-e2e` `signal_attachments` on the mock: the first photo
    download ended `Failed { retryable: true }` within 4 s and the test, which allows no
    failed state at any time, failed. Not reproduced and not explained; if `step
    attachment-receive` times out in the real run, the log's `cdn` request line says why.
- Open for the next phase:
  - First: the real-server run itself, with the user (`docs/smoke-run.md`). Its section "What
    the mock could not show" lists what only that run answers.
  - The group of the run needs at least two other members for a sender-key send. With the test
    account and the personal account alone the group send goes out pairwise and the
    multi-recipient request stays untested on a real server; `step group-send` says which
    happened.
  - "Remove account" and "Link again" have no dev-control command and no smoke step: by hand in
    the window, on purpose only.
  - `kourier-app/tests/secret_service_keys.rs` writes to and deletes from the session's real
    Secret Service (items of a profile name made for the run). It is part of `cargo test -p
    kourier-app` and of `cargo xtask ci`.
  - The windowed path of `smoke start` (a real display) was not run: every run here was
    offscreen.
  - The phase J list above stays open.

Phase L (the first run against Signal's production servers, 2026-10-07, with the user: a
dedicated test account linked as device 2, his personal account as the peer, the login kept in
the smoke profile). The run and its numbers are in `messengers/NOTES.md`; what it changed:

- Passed on production: link by QR code with the Storage Service sync, reconnect on the kept
  login, receive and send 1:1 (sealed sender both ways, delivery and read receipts), a group
  message in and out, a photo from CDN 3, an upload of a picture and a file through the gRPC
  upload form, reaction, quoting reply, edit and delete for everyone in both directions, a
  disappearing timer set by the peer, and the catch-up of three messages sent while the app was
  stopped (once each).
- The server writes the signatures of a prekey bundle as base64 WITHOUT padding (the mock
  pads). Every base64 value of a server response is decoded by `kourier-signal/src/wire_b64.rs`
  (`LENIENT`: with or without padding); a strict decoder in a new parser is a production-only
  failure that no mock test shows.
- §6.5 step 8: a sync request of the link (CONTACTS, CONFIGURATION, BLOCKED) that is dropped
  (a failure no retry mends, or its day ran out) clears `account.app_version` in the
  transaction that deletes the job (`jobs::retry` calls `maintenance::job_dropped`), so the
  next start asks for all three again. Before, the mark was written when the requests were
  queued and a request that failed once was lost until the next Kourier version: in this run
  the padding failure dropped all three. `backend.db` schema 14 clears the mark once for
  profiles made before (they cannot tell whether their requests went out). Not at once but at
  the next start: a permanent failure fails again until a new build or new server data arrives.
  The same holds for the job that applies the phone's answer to CONTACTS (`contact_sync`): when
  its blob cannot be fetched or read, the request went out and the contacts did not come. A
  re-ask renews a request that is still queued (`jobs::enqueue_renewed`: due now, no failed
  attempts, a new day), so a row left from days ago is not dropped at its first failure. Not
  asked again: an answer that is malformed in itself (no payload, no usable blob pointer).
- `cargo xtask smoke`: the catch-up step counts stored rows from others with an id above the
  newest one remembered before the stop, chat by chat (`stored_after`), not unread counters:
  those fall when the user opens a chat in the window or reads it on the test phone. The read
  step asks for the window's focus and waits (a chat is marked read only in a focused window);
  the modifiers step takes a reaction by someone else on any row (a short timer may have
  erased Kourier's own messages).
- Open from the run:
  - The multi-recipient (sender key) send is still untested on a real server: the run's group
    has two members.
  - `step receive` and `step group-receive` still count by unread growth and misread when the
    user clicks another chat while they wait.
  - With a disappearing timer of seconds Kourier's own messages are gone before `react` and
    `reply` find a target: run the modifier steps with a timer of minutes or none.
  - The first prekey rotation on the real server comes two days after the link; the 30-day
    idle limit of a linked device is still unconfirmed.

Phase M (what the user saw in his first session on the desktop, 2026-10-07; the scrolling of
the timeline: "I was trying to scroll up but app actively returned me to bottom"):

- The fault. `TimelineView` took the user's scrolling from the list's own movement
  (`onMovementEnded`) and from a scroll bar that was pressed or hovered. On the desktop the
  wheel and the touchpad do not move the list that way: the `org.kde.desktop` style's
  `ScrollView` holds a `Kirigami.WheelHandler` that takes their events and writes `contentY`
  (animated over `Kirigami.Units.longDuration`, at once when smooth scrolling is off), so the
  list never reports `moving`. `followNewest` stayed true, and the next change of the content
  height (a delegate made while scrolling up, an arrival, a receipt) put the view back at the
  newest message. No test showed it: the UI tests run with the default Qt Quick Controls
  style, where the wheel goes to the Flickable and the Flickable reports a movement.
- The rule now (`TimelineView.qml`, the comment above `own` in the list): every change of
  the view's position that the view did not make itself is the user's, whatever moved it
  (the wheel handler with or without its animation, the scroll bar's handle or track, a drag
  or flick, a key that makes a row current). The position is the viewport's bottom edge in
  content coordinates. A resize is no move: a BottomToTop list changes `contentY` with its
  height to keep that edge, before it says that its height changed; within the resize's
  distance of the end of the content the bounds it checks are still those of the old height
  and the edge does move, so every change that comes before `heightChanged` is the list's.
  A move toward older rows that leaves the view away from the newest message stops
  following at once. A view that rests at the newest message, with no newer page to load,
  follows again (`rest`: a moment after a move, and when a drag or flick ends). The view's
  own moves (to the newest row, to a quoted message, to the oldest row with `pinToOldest`,
  back to what it holds on to) are marked and decide nothing. The scroll bar properties
  `barPressed` and `barInUse` are gone, and a current row no longer switches following off
  by itself: where the view is decides, also for the keyboard.
- What the view holds on to while its content changes. While it follows: the newest message.
  Otherwise the topmost row in the viewport, by its top edge (`held`): after a change the
  view is put back so that row is where it was (`settle`, asked for by the model's
  `rowsInserted`, `rowsRemoved` and `dataChanged` before the list lays the change out, and by
  the content height, the list's height and the held row's `y`). A list alone does not keep a
  reader's place: a row inserted at index 0 takes the place of row 0 when row 0 reaches into
  the viewport and pushes the rows up (a newest message taller than the window is always in
  the viewport), and a row below the viewport that changes its height moves everything
  above it, because a BottomToTop list lays its rows out from the newest one it has made. So
  for a reader a row grows downward, and a window that changes its height keeps its top.
- Not solved. The wheel handler animates to positions it fixed when the event came: a row
  that arrives or goes during the 200 ms of that animation moves the rows once by its height
  (the view cannot correct a position the next animation step overwrites). A change that
  replaces the model's rows (`diff_or_reset`: more than 16 rows and more than half of the
  list at once, for example many arrivals after a reconnect or many messages that disappear
  together) still starts the list again at its newest message, also for a reader who had
  scrolled away: a first attempt to keep the row (no reset while a row stays) is kept as
  `target/ui-context/reset-rework-unfinished.patch` and was not finished. The
  first Up or Down in a timeline that has no current row makes the newest message current
  and goes there, wherever the view was (the keyboard design of phase G).
- The test, `crates/kourier-qt/tests/scroll_ui.rs`: the real `MainWindow` in a `QApplication`
  with `QT_QUICK_CONTROLS_STYLE=org.kde.desktop` and `QT_QPA_PLATFORMTHEME=kde`, real wheel,
  mouse, key and touch events (`TestEvent`), one session run in two child processes with a
  `kdeglobals` each: `SmoothScroll=true` with clicks on the scroll bar's track that go to
  the place of the click, and `SmoothScroll=false` with clicks that page
  (`ScrollbarLeftClickNavigatesByPage`). It asserts that the `ScrollView` has the wheel
  handler, that the handler took every wheel event and that the list reported no movement
  for them. The session: a chat in which a message arrives every 0.6 s and disappears 0.3 s
  later; a message three windows tall is the newest one; the wheel goes up through it and
  past it in steps smaller than the window (single turns of four notches, and eight small
  steps as a touchpad sends them); after each step the view stays, the row being read does
  not move by more than a pixel, "Jump to Latest" shows, and every line of the message was
  in view on the way; a
  reaction makes a row taller; the window gets smaller and larger; the scroll bar's track
  and handle; the wheel back down, where the view follows again, also through a resize; rows
  made current by the keyboard; "Jump to Latest"; another chat, up to its oldest message
  while older pages load; a finger that drags and flicks. On the tree before the change it
  fails at the first wheel step: "after the wheel: the view went back to the newest message
  (following true, … wheel events sent 1 and taken by the style's handler 1, movements the
  list reported 0 …)".
- Fixture and fake, for that test: `FixtureOptions::arrivals` (`kourier_app::Arrivals`)
  appends to the fixture script what happens after its data: the peer of the showcase chat
  with the timer sends a message every `every`, read on arrival and gone `lifetime` later.
  A start waits for the data steps only (`FakeControl::ran_steps`).
- The other scroll views (the conversation list, the settings page, the composer's file
  list and text area, the emoji grid, the dialogs' lists, the viewer) keep no rule of their
  own about where they are: none follows an end or restores a position, so none has this
  fault.
- Read more (long texts; the user: "I sent long text and app showed it as long message
  without collapsing"). A text shows its first 800 grapheme clusters; when no more than 100
  would be left it shows all of it; each "Read more" adds 3,000 and again shows all when no
  more than 100 would be left. This is Signal Desktop's rule (`MessageBodyReadMore`,
  `graphemeAndLinkAwareSlice`: 800, 3,000, buffer 100), counted in grapheme clusters instead
  of UTF-16 units (`unicode-segmentation`). The cut is made in
  `kourier-viewmodel/src/readmore.rs` on the text and its ranges, before the RichText is made,
  never on HTML. It never falls inside a grapheme cluster, a link (a bare URL or a link range)
  or a mention: it moves to the end of the one it falls into. A formatting range (bold,
  italic, strike, monospace, spoiler) that crosses it ends at it, so the shown part is a
  `Text` of its own and its markup is well formed. As in Desktop the cut can fall inside a
  word; the markup of a cut text ends with an ellipsis (not part of `plain`). Units are
  counted only up to limit + 101. `RichText` has `more` and `whole`: in the QML text block
  `html` and `spoilers` are the shown part, `plain` is always the whole displayed text (the
  menu's Copy, the row's accessible name; `RichText::full_plain()` is what an edit starts
  from). A text longer than 800 bytes is rendered once and kept (`ReadMore`'s cache: made
  again only when the text, the limit or the display names of the people it mentions change),
  so the timeline's frequent rebuilds do not format long texts again. State: `ReadMore` (the
  shown length per message id) lives in `CoreSource` next to the open chat's window and is
  cleared whenever another chat is opened; it survives delegates being destroyed, row updates
  and window reloads. The invokable `TimelineModel.readMore(messageId)` raises one step and
  the row changes in place (one `dataChanged`, never a reset). UI: `TextBlock` shows a "Read
  more (N words)" label under the text in the link colour, an accessible button (N: the words
  that are not shown, `RichText::more_words`, counted the simple way as what stands between
  spaces and line breaks: user decision, 2026-10-08); a click goes through
  `TimelineView`'s `list.readMore`, which stops following and calls the view's own `hold()`
  before the text changes, so the line where the text was cut stays where it is on the
  screen and the new text continues below it. Enter or Space on the current row whose text
  is cut expands it first. It applies to every text block (own and others' messages, with a
  quote above, as a caption); a long-text attachment whose rest has not arrived shows what is
  there. Fixture: the showcase chat of `--page=readmore` (one received message of 1,645
  characters); the fixture now has 20 chats (the page `unavailable` of step S5 adds none: its
  rows are a history batch in a generated chat of the list's lower body). Tests: unit tests in `readmore.rs` and
  `kourier-qt/tests/readmore_ui.rs`. Not solved: in a chat shorter than the window the hold
  cannot keep the cut line (nothing to scroll), the text grows upward and the view follows
  the newest message again; chats with a history are not affected. The row's menu has no
  "Read more" entry.
- Screenshot step (`cargo xtask shots`, `xtask/src/shots.rs`; the user: "comparison with
  previous ones and only in case of some difference we should check it"). Before, the 152
  pictures were rendered one after another (10.2 minutes of a 22-minute CI run) and nothing
  compared them with earlier ones. Now 4 app starts run at a time (`--jobs N`; 247 s
  measured on an idle machine; since T1, 2026-10-13, one per core up to 6: 266-276 s against
  315 s at 4 on a 6-core machine with one core busy). Renders share nothing: each fixture start has its own
  temporary profile, takes no D-Bus name, opens no port and uses a passphrase keyring; the
  Breeze `kdeglobals` of each colour scheme is written once before the first render; only
  KDE's icon cache in the real `~/.cache` is shared (made for several processes). Every old
  check applies to every render. The render's `kdeglobals` has `[KDE] CursorBlinkRate=0`, so
  the text cursor of a focused field does not blink (without it 3 of 6 renders of `edit`
  differed by the 36 pixels of the cursor). Comparison: a render into the default directory
  is compared with the picture of the same name in `target/shots-baseline` (local, not in
  git). A pixel differs when any channel differs by more than 24; a picture differs when more
  than 24 of its pixels differ or its size does (an absolute number: a share of the picture
  let a changed label pass on the wide pages). The eleven pages that show pictures
  (`SHOT_PAGES_WITH_PICTURES`) are compared more loosely, at 0.15 % of their pixels: a page
  is grabbed when it is final (S1, below), but a picture asked for before its thumbnail
  existed is drawn from the file and one asked for after from the thumbnail, which differ by a
  level or two in a fifth of the pixels and by a few pixels at the edges of shapes. Open:
  compare these pages as strictly as the others once three full runs show no re-render of a
  picture page; a picture that differs is rendered up to
  twice more and counts as changed only when every render differs (a page that is still
  loading when it is grabbed differs by a row now and then). `qr` (random content) is not
  compared. The run prints "N pictures, M changed: names", writes
  `target/shots/diff/<name>.png` for each changed picture (baseline, render, differing pixels
  marked) and fails naming them and the accept command. `--accept` makes the pictures just
  rendered the baseline, `--no-compare` only renders, a render into another directory does
  not compare, `--page/--theme/--size` render a part, `--no-build` uses the binary that is
  there. Kirigami's own drawer handle reports a binding loop on the narrow `edit` and `reply`
  pages in some parallel renders (never in serial ones): `qml_problems` passes over exactly
  those two lines; every other QML line still fails the render. Skipping: `cargo xtask ci`
  skips the step when a digest equals the one stored in `target/shots/.stamp` by the last
  full, successful, compared (or accepted) render that built the binary itself. The digest
  covers every file under `crates/` except `kourier-signal`, `kourier-e2e`,
  `kourier-coexist`, `tests/` and `target/` directories, plus `packaging/`, `Cargo.lock`, the
  two xtask files, the Qt version and the two Breeze colour scheme files. Not covered: a
  changed font or Breeze package (`cargo xtask shots` never skips). Known unstable spots,
  measured under load: a late chat-list row in wide `edit`/`reply`, and the album pictures of
  `deleteattachment` (two states; fixed by S1 below).
- Stable screenshots, S1 (2026-10-13; the design is the working note
  `target/next/stable-screenshots.md`). A `--screenshot` run no longer grabs a fixed time after
  its page is ready (1,200 or 2,500 ms): `MainWindow.qml`'s `shotTimer` checks every 50 ms,
  after a floor of 300 ms, that nothing holds the picture back and then saves the second of
  two equal frames. Holds: images loading in the window, requests of the image provider
  (`pendingImageRequests`), thumbnails the core still derives (`MediaHandles::deriving()`:
  reserved in the same function that publishes a change with derive jobs, BEFORE its first UI
  event, and released per derive task after the group it recorded was published, by a guard
  that also releases on every early return), downloads running, the page row moving, the link
  page still in the stack on the link pages of a transfer. At 30 s the frame is saved and the
  app prints `shot: unsettled <what>` (a failed render for `cargo xtask shots`). Cost: every
  page waits for the fixture's thumbnails (a full render 430 s, was 270 s). Measured with
  `cargo xtask shots --repeat 10` under a second render (load average 42): 480 renders of 12
  pages, 25 hit the old 15 s cap (now 30 s), 15 of 48 pictures had a second picture among the
  settled renders (one or two levels inside photos: a picture requested before its derive is
  drawn from the file, one requested after from the thumbnail, and its URL does not change when
  only the thumbnail appears, so it is not requested again). The one-run check of the design's
  cause for the transfer pages (the ColumnView still sliding) printed `moving=false
  contentX=0` at the grab of all 16 renders, one of which differed from the rest by one grey
  level: the slide was not the cause there; the equal-frames check is what settles those.
- Waiting times, measured for this phase (a full CI was about 22 minutes: 10.2 screenshots,
  about 6 the 86 non-Qt test binaries one after another, about 5 building, 1.5 Qt tests). A
  read-only audit (`target/wait-audit/*.md`) found little real duplication between the test
  layers; what it lists, not done yet: test binaries in parallel (about 200 s), the fixture
  profile seeded once per screenshot run, a shorter fixed wait before a screenshot (done by S1: none),
  `kourier-core`'s test files as one binary, the two kill-at-every-failpoint loops on 7
  threads, and a 20 s fixed wait in the rehearsal's `step send`.

## 14. FOSS readiness track (user requirement, 2026-10-06)

Goal: Kourier becomes a community project people can trust, install and improve: public releases,
reproducible builds, packages for several platforms, translations anyone can contribute to, KDE
guidelines and theming that really follows the system. This track runs alongside M1 and gates the
**first public release (R1)**. Nothing is published, pushed or submitted without the user's explicit
go in that session. Research with sources (working notes outside this repository):
`foss-project-release.md`, `foss-packaging-reproducible.md`, `foss-translations.md`,
`kde-guidelines.md`, `theming-audit.md` (+ `theming-audit-files/`).

### 14.1 Decisions

| Topic | Decision | Why |
|---|---|---|
| Name | **Rename before R1** (user, 2026-10-06). "Kourier" collides with KoreTech's commercial "Kourier Messenger" and CNCF Knative's Kourier. A conflict-checked shortlist goes into `notes/research/name-shortlist.md`; the rename (app id, D-Bus name, QML URI, crate names, icon) is one mechanical step before R1 | Cheap now; later it resets notification/autostart settings and needs a Flathub migration |
| Hosting | **GitHub**, KDE-compatible (REUSE, KI18n/gettext, AppStream, KDE HIG, CoC); KDE incubation decided after R1 | KDE licensing allows AGPL only for server software (libsignal forces AGPL here), KDE CI needs CMake, incubation wants more than one developer |
| License | AGPL-3.0-only; DCO sign-off, no CLA | Inbound = outbound; libsignal is AGPL-3.0-only |
| AI provenance | **Public AI-use statement** (user, 2026-10-06): `AI-USE.md` + README section (what is AI-written, how it is reviewed and tested); keep the co-author trailers. The Flathub manifest and the Flathub PR are written and submitted **by the user by hand**; a self-hosted signed Flatpak repo is the fallback | Flathub requires disclosure, forbids AI-written manifests and AI-driven submissions; acceptance is at reviewer discretion |
| Signal trademark | Never "Signal" in name, icon, domain or handles; describe as "an unofficial, independent client compatible with Signal … not affiliated with or endorsed by Signal Messenger, LLC"; honest user agent; **redesign the icon** (today's blue rounded square with a white bubble is too close to Signal's); `.desktop` Comment becomes "Unofficial messenger compatible with Signal" | Signal brand guidelines; LibreSignal precedent; Flare/Molly/Whisperfish wording |
| First channel | **Flatpak bundles on GitHub releases + a signed self-hosted Flatpak repo**, then **Flathub** once there is a visible history of several tagged releases; AUR and a Nix flake soon after; OBS deb/rpm, Snap, AppImage later; Windows/macOS later (Craft); Debian/Fedora main archives not a goal (libsignal git deps, vendored BoringSSL) | Where Linux desktop users install apps; a dynamic Qt/KF6 tarball is not portable |
| Translations | gettext PO, domain `kourier`, one catalog for QML + Rust + `.desktop` + metainfo; Rust strings through KI18n via a small C++ shim behind a Qt-free `Translator` trait (not gettext-rs: KCatalog's env handling races with libintl on tokio threads); KDE `%1` placeholders, FSI/PDI isolation for user text; GNU xgettext ≥ 0.24 (Rust support); ICU4X for date/number formats in the viewmodel; **Hosted Weblate (Libre plan)** once public | Standard tooling translators know; KDE path stays open; Weblate is libre and keeps translator authorship |
| Theming | Follow the system completely: drop the forced `QT_QUICK_CONTROLS_STYLE`, call `KirigamiAppDefaults::apply` (style incl. Union, color schemes and icons outside Plasma); **own bubbles = accent tint** (normal background tinted 15-20% with the accent, normal text; user, 2026-10-06), all text ≥ WCAG 4.5:1 via a `Style.readable()` helper; themed app/tray icons by name; Breeze icons as fallback off Plasma | Audit: solid accent bubbles 2.5:1, disabled-text role misused for secondary text, avatar colors as text 1.27:1, no icons without the KDE platform theme |
| Tray | **Off by default** (user, 2026-10-06); opt-in in Settings | KDE HIG; NeoChat precedent |
| Version | SemVer from 0.1.0; `CHANGELOG.md` with git-cliff from conventional commits | Common expectations; machine-generated release notes |

### 14.2 R1 gate: must be done before the first public release (dependency order)

1. Name chosen from the shortlist and applied everywhere; new icon (not Signal-like) as a themed SVG
   plus a monochrome `-symbolic` tray icon; app id frozen (it needs a domain or account the project
   controls for Flathub's verification, for example `io.github.<account>.<Name>`; today's
   `app.kourier.Kourier` is a placeholder).
2. KF6 dev packages installed by the user (`kf6-extra-cmake-modules kf6-kirigami-addons-dev
   kf6-kcoreaddons-dev kf6-ki18n-dev kf6-kcrash-dev kf6-kiconthemes-dev kf6-kcolorscheme-dev`; agreed
   2026-10-06) and a `cpp/kde.cpp` shim: `KirigamiAppDefaults::apply`, KI18n localized context, KAboutData
   (About page via `FormCard.AboutPage`, KCrash with a redacted panic message), window icon by name;
   delete `prepare_environment()`.
3. Theming fixes from the audit (A1-A5 contrast, B frames from `frameContrast` for high contrast, C
   hard-coded colors → Complementary color set in viewer/overlays, D fonts incl. `fixedWidthFont`, E
   heights from content at 14 pt, F icons by name, live recolor on scheme change) and an extended
   `cargo xtask shots` matrix: Breeze Light/Dark/Classic, Oxygen, two high-contrast schemes, odd accent
   colors, Union style, 14 pt font, RTL (`LANGUAGE=ar`), no animations, a non-KDE run, plus an automatic
   ≥ 4.5:1 contrast check on own-bubble and secondary text.
4. KDE HIG pass: Title Case for buttons/titles, hide disabled stub actions, keyboard shortcuts (send,
   new line, search/quick switch, next/previous chat, mark read, …), message context menu (right click,
   Menu key, long press) instead of hover-only Reply, accessibility minimum (accessible names for
   messages and controls, focus order, a manual Orca pass), tray off by default.
5. Repo files: README (screenshots, status, install, comparison), `LICENSE` + REUSE/SPDX headers +
   `LICENSES/` (`reuse lint` in CI), `CONTRIBUTING.md` (DCO), `CODE_OF_CONDUCT.md` (KDE CoC or
   Contributor Covenant), `SECURITY.md` with a private disclosure channel, `AI-USE.md`,
   `GOVERNANCE.md`/`MAINTAINERS`, issue and PR templates, labels (`good first issue`), privacy statement
   (no telemetry; what is stored and how it is encrypted), accessibility statement.
6. Supply chain: GitHub Actions with actions pinned by SHA and minimal permissions, `cargo deny`
   (licenses, bans, advisories), `cargo audit`, Renovate or Dependabot, CycloneDX SBOM, third-party
   license notices generated with cargo-about (libsignal pins 0.8.2) and installed under
   `share/licenses/<id>`, OpenSSF Scorecard; fuzz targets (cargo-fuzz) for proto/envelope/attachment
   parsers run in CI for a short budget. Runner disk is 14 GB: CI needs cache pruning or builds inside
   the Flatpak SDK.
7. AppStream metainfo `<id>.metainfo.xml` (developer id, OARS `social-chat intense`, branding colors,
   homepage/vcs/bugtracker/translate URLs, `<translation type="gettext">`, releases, 3-6 real screenshots
   ≤ 1000×700) passing `flatpak-builder-lint appstream` and `appstreamcli validate`;
   `desktop-file-validate` keeps passing.
8. Flatpak: `org.kde.Platform//6.11` + rust-stable + llvm + protoc, offline cargo via
   flatpak-cargo-generator, a spike proving BoringSSL, bindgen, cxx-qt (rcc, qmlcachegen) and SQLCipher
   build offline on x86_64 and aarch64; test against the runtime's Kirigami (6.28 there, 6.30 here);
   sandbox paths: Secret portal (oo7), portal file dialogs, Background portal for autostart, ksni with
   `disable_dbus_name`, direct Notifications with a written justification, badge check.
9. Toolchain policy: pin `1.99.0`, keep `rust-version` ≤ the Flatpak rust-stable version, never set
   `RUSTFLAGS` in packaging (the lld flags stay in `.cargo/config.toml` for dev only).
10. Reproducible builds: `SOURCE_DATE_EPOCH`, Rust and C path remapping (later
    `profile.trim-paths`), deterministic rcc, `cargo --locked`/`--frozen` with a vendored tarball,
    version string without git; CI builds the Flatpak `/app` tree twice on different runners and fails
    on any difference; `REPRODUCIBLE.md` with a rebuild script and per-release digests (model:
    Signal-Desktop `reproducible-builds/`).
11. Translations: KI18n-backed `tr*` in the viewmodel, `cargo xtask i18n extract|build|check`
    (xgettext for QML + Rust, `msgfmt --desktop/--xml` for the `.desktop` file and metainfo),
    committed `po/kourier.pot`, `po/LINGUAS`, a pseudo-locale (x-test) and RTL shot, a translator guide
    (`TRANSLATING.md`: Weblate link, context markers, plural rules, how to test a language locally),
    then the Weblate Libre component (file mask `po/*.po`, msgmerge + linguas add-ons, translation
    license AGPL-3.0-only).
12. Release pipeline (`cargo xtask release X.Y.Z` → signed tag → `release.yml`): source and vendored
    tarballs, Flatpak bundles x86_64/aarch64 with `--override-source-date-epoch` and lint, the
    reproducibility double build, license notices, SBOM, `cargo deny`, `SHA256SUMS`,
    `REPRO-DIGESTS.txt`, build-env record, `actions/attest` provenance over every asset, a **draft**
    GitHub release published by the user, immutable releases enabled, AGPL source offer satisfied by the
    attached source tarball.
13. Real-server smoke runs with a dedicated test account (user OK required) and the manual Plasma
    checklist (§10.6 and the S14 review list).
14. History transfer (user, 2026-10-08, answering "before or after the first release?":
    "Before"). (a) A newly linked device takes Signal's own transfer at link time ("We should
    support it before first release"): `backup5` in the QR, the phone uploads one encrypted
    archive, Kourier downloads, decrypts and imports it; opt-in at link time. (b) A device
    that is already linked ("We should support it before first release"; merge or replace:
    "Replace but inform user"): the protocol has no request for old history, so Kourier
    offers to link the account again with a transfer, which REPLACES the account's local
    history as Signal Desktop does, after telling the user plainly what is replaced. Not
    in scope: an import from Signal Desktop's local database ("Not for now") and from
    backups ("No"). What the sources say, the pieces and the traps:
    `notes/research/history-import.md` of the project notes (to become a section here as
    it is built). Design: the archive is asked for on every FRESH link (the phone asks the
    user, so Kourier has no question of its own) and never when a logged-out account that
    kept its chats is linked again; "already linked" is an entry "Transfer History from
    Phone…" that, after a plain warning, removes the account from this computer and links
    it anew with a transfer. Built so far (2026-10-08): the reader of the archive file,
    `kourier-signal`'s module `archive` (libsignal-message-backup at the pinned tag for
    the cryptography; own framing, prost types from `backup.proto` and limits; a writer
    for tests; proven against libsignal both ways). Its review found that libsignal's
    crate trusts its input in two places a downloaded file must not be trusted in, and the
    reader now refuses the header format of remote backups and counts a frame's fields
    before decoding it. The link-time protocol is built too (2026-10-09,
    `kourier-signal/src/transfer.rs`, backend.db schema 16 and 17) and its review's
    findings are fixed. IT IS IN USE BY DEFAULT since 2026-10-11 (the user, asked with
    options: "On by default (Recommended)"): every fresh link offers `backup5`, like Signal
    Desktop, and the phone asks its user whether to transfer, who can decline there. It was
    held back until the import and its screens existed, so that no phone was made to export
    a history that would be read and dropped. The app switches the offer on where it builds
    the Signal network (`Tuning::transfer_offer` through
    `SignalNetwork::with_history_transfer_offer(true)`, which also sets the descriptor's
    `HistorySource::PushedOnLink`); the library's own default stays off, because the tests
    that build a network directly (the mock harness) are not about the transfer. With the
    offer off the linking URL carries no capability and a key that arrives anyway is
    ignored. As built:
    - One row per account holds the state: waiting, downloading, reading, done, skipped,
      failed, with a reason. Receiving and the Storage Service sync wait until the
      transfer is over; every end lets them start.
    - Waiting long-polls `GET /v1/devices/transfer_archive`, five minutes per request and
      an hour in all. A 204 that was not held waits for the reconnect backoff. A 4xx other
      than 408 or 429 ends the wait as skipped. An error value this version does not know
      fails as "update Kourier". The deadline is a wall-clock time: what is left is
      clamped to the whole wait, and the server is asked once more before a wait ends by
      it.
    - The download is resumable, capped at 8 GiB, fails after an hour without a byte, and
      is bounded as a whole: two hours of RUNNING time (stored with the progress, so a
      restart goes on counting and the time the application was closed does not count),
      or an average under 4 KiB/s for ten minutes after the first minute.
    - A skip is honoured in waiting and downloading, also while the downloaded file is
      synced. The read hands frames to an `Importer` and cannot
      be skipped. It is started at most five times, counting only reads that ended with
      the process. Every failure is final: the way to try again is to link again.
    - The key and the archive's place leave the row in the write that ends the transfer
      and leave the WAL by the checkpoint that follows it; they also leave at the next
      start after a kill between the two, and before the actor ends when the account is
      logged out during a transfer. Proven page by page on the two database files
      (`tests/history_transfer_attack.rs`).
    - The stored state is reported at every start of the account and at every connect,
      also an end, until `DismissHistoryTransfer` removes it: a front-end shows a state
      idempotently, and the bytes of a download may step back after a restart. The
      contract is in `kourier-model/src/transfer.rs` and `kourier-core/src/facade.rs`.
    - The 60 seconds the phone waits for the link: on the mock 0.34 s from the
      provisioning message to the answered link request, 96 % of it opening the
      unauthenticated socket (libsignal's 300 ms happy-eyeballs delay). The QR sockets
      are closed beside the request, not before it. Against the real server it is
      unmeasured (the log line "device linked" carries `envelope_to_linked_ms`).
    Known and left: when the deadline of the wait passes while the account is offline,
    the last question waits for a connection (the state stays waiting until then or a
    skip); a stop while the reader opens a very large archive waits five seconds and then
    counts as a cut-off read; a stop stores nothing, so up to a second of a moving
    download's running time (30 s of a stalled one's) is not counted.
    The import is built up to its commit (2026-10-09, `kourier-signal/src/transfer/`,
    backend.db schema 18) and its review's findings are fixed; the design and its decisions
    are `notes/research/history-import-mapping.md` of the project notes. As built:
    - While the archive is read, its frames are mapped (`transfer::map`, pure) into rows of
      the contract and written to the stage (`transfer::stage`): `transfer/stage.db`, a
      SQLCipher file beside the archive whose key is HKDF-SHA256 of the transfer's ephemeral
      key, so the write that ends a transfer makes a stage that is still on disk unreadable.
      One row per message, keyed by chat and frame number, so a chat comes back in frame
      order whatever the interleaving; people and chats are staged when a row first uses
      them. The reading thread writes that one file and nothing else.
    - The commit is one write in `backend.db` after the reader checked the archive to its
      last byte: `import_staged = 1` with the stage's totals and the archive's frame count.
      Before it, an archive that fails and a kill leave nothing that is ever used: the stage
      is removed by the abort, or by the next start before its read begins, and the archive
      is read from its first frame. A quit leaves nothing or a recorded stage: a read that
      is stopped between two frames removes its stage, and a read that had come to its end
      when the stop reached it is recorded by the stop itself, so the next start goes on
      after the commit. (A read that does not end within the stop's five seconds leaves an
      unrecorded stage, which the next start removes.) A stage that was recorded is not read
      again; one that is recorded and lost, damaged or different has its record taken back
      and its file removed, and the archive is read again.
    - This step maps: our own account, contacts, the release notes (dropped with their chat,
      as in Desktop), 1:1 chats and Note to Self, and plain text in both directions with its
      id (author and original sent time, as the live path), times, read state, the
      aggregated send state and the disappearing timer. This computer's clock decides
      nothing: no row is skipped for being due and no timer's start is moved back to the
      import's time; only a time more than a day ahead of the import is replaced. Two
      contact frames of one person are one person and one conversation. An own message's
      state is what the phone says for the chat's contact. Everything else is skipped and
      counted under a reason; one item never fails an import.
    - Proven on the mock for everything before the commit: the core gets no
      event of the import and `core_outbox`, `peers`, `groups` and `jobs` are unchanged at
      the commit while the stage holds the archive's rows; and page by page that an aborted
      import leaves no text in `backend.db`, its WAL or the transfer directory.
    - Measured (debug build, one run): 50,000 text messages over 200 chats are verified,
      read, mapped and staged in 4.3 s; the stage is 31 MiB (517 bytes a message).
    **Delivery (S2, backend.db schema 19; 20 since S4b).** After the commit zone the stage is
    delivered to the core (`transfer/deliver.rs`): the people, then the chats that are shown
    (upserted the least recent first so that the list stands in the phone's order before it
    fills, each with its features), then each chat, the most recent first, as `HistoryBatch`
    rows of at most 500 messages and 1 MiB with `has_more` and `progress`, (a chat whose last
    row is not delivered still hears that nothing follows). No unread count is sent: the rows
    say whether the phone had read them, and the core counts the unread ones. One zone per row
    writes the backend's own rows, the `core_outbox` row, its entry and the cursor, so a quit
    or a kill goes on at the cursor and every row is appended exactly once; at most two rows
    wait for the core. `done` is written after the last acknowledgement, the login's `Done`
    after that. A committed import is never withdrawn: a lost stage is read again from the
    archive (ids are deterministic, ingest is idempotent), also for a rebuilt `core.db`. A row
    the core refuses or does not acknowledge within the bounds is appended again as single
    messages; only the oldest waiting row is ever dropped (the core stops at the first failing
    job), and a single that still fails is dropped and counted. A delivery that cannot finish
    ends as the failure kind `Incomplete` with what was delivered left in place and receiving
    started: when the core answers nothing three times in a row, when it refuses 20 single
    units in a row, and when the time spent waiting for rows that were then taken back reaches
    an hour in all. A dropped unit alone leaves the transfer `done`. A logout or an unlink
    during a delivery keeps what was delivered. The chat of a contact known only by a number
    is read-only. A row that is due is dropped only under a clock the server confirmed on the
    connection that stands. Measured in a release build (the builder's run, before the fixes):
    50,000 texts over 200 chats are read in 0.6 s and delivered in 1.9 s; stage 30 MiB,
    core.db 23 MiB, backend.db at most 1.6 MiB. Not yet: attachments (S5; content beyond plain
    text came with S3b, what roams with S4a and groups with S4b, below); a chat all of whose
    rows were due stays in the list, empty; at this step the offer stayed off by default (it
    has been on by default since 2026-10-11, below). A rebuild of `core.db` after the transfer
    ended cannot bring the history back (the archive and its key are gone).
    **The rules of the contract and the core (S3a, core.db schema 12, `stage::FORMAT` 2).**
    The lead's rule replaces the design's C2: the rows are the truth about what is unread. A
    history row that says "not read" counts for the chat like a live row and is announced to
    nobody; reading the chat here clears it for good (also while the rest of the chat is
    delivered, and when a lost stage makes every row come a second time); a read sync of the
    phone lowers the count by the messages it names. The delivery sends no count. Reading an
    imported chat tells our other devices about each imported message that was unread and
    names the newest message of the chat, never a notice; the sender of an imported message
    gets no read receipt (a known difference from Signal Desktop, D9). An imported message
    that was edited carries its versions' ids (`IncomingMessage.edits`), so the mark shows
    and later modifiers that name the newest version find it. Three timer cases: an own
    message that was pending or failed on the phone keeps its timer without a start and the
    phone's transcript of it starts it; an own message the phone calls sent whose timer never
    started keeps its timer, started at the import's time (never without a timer, which would
    stay for ever, and never from the sent time, which would erase what the phone still
    shows); a call line of the history takes the timer and the read state the archive states;
    a notice whose after-read timer never started starts when it is stored. Proven with the
    real core in `kourier-core/tests/history_rules.rs` and
    `kourier-e2e/tests/signal_history_import.rs`. The partial index of unread rows is
    `read_state = 'Unread'` alone, so a read marker of the phone costs the unread rows, not
    the chat's length (200 markers on a chat of 20,000 rows: 3.13 s to 0.26 s, debug). Known
    and left: a row that is delivered after the chat was read here and says "not read" counts,
    also when it is older than what was read (a row taken back and appended again); a receipt
    alone does not start the timer of a message that was pending on the phone; a call line that
    still rings when imported does not keep `read: true`; a version id equal to another
    message's id of the author takes what waited for that id.
    **The content of 1:1 chats (S3b, `stage::FORMAT` 3).** The mapper now maps, in direct chats
    and Note to Self: formatting and mentions (the archive's UTF-16 ranges through the live
    mapper's `receive::dispatch::ranged_text`; a range that does not fit is dropped and counted,
    one that crosses the cut of a long text is shortened to the cut and dropped only when
    nothing of it is left; a mention of us sets `mentions_me`); quotes (the target by its sent
    time, `local` "0" when the phone did not find it, the author as far as the chat knows them,
    the quote's own text and its first attachment's kind, name and type, no thumbnail);
    reactions (one per author, the highest `sortOrder` wins, in that order, ours as `by: None`,
    the live emoji rule); an edited message's later versions as `IncomingMessage.edits` (its id
    stays the first version's; earlier texts are not imported; sorted by time, one entry per
    time, the newest last, whatever order the archive lists them in); a sticker as
    `Body::Sticker` whose image follows the attachment rules of S5, its content
    type kept as the live path keeps it (`receive::media::sticker_mime`: none is `image/webp`,
    one over 255 bytes `application/octet-stream`); view-once (`Unsupported "view-once"`,
    `view_once`, no attachment and no raw), contact cards, payments, gift badges, polls and
    story replies as the live path's placeholders with the encoded backup `ChatItem` as raw
    (`BackendBlob` v 4, at most 64 KiB); the 1:1 notices the contract has (timer change, safety
    number, number change, session reset, accepted request) under ids `h:<frame>:0`;
    newer-protocol, payments activated, poll ended and pin as placeholders under the message's
    real id; and 1:1 call lines in every state, said by our own device (`call_id` the archive's
    `callId`, else `h<frame>`, and a `callId` of 0 is none, as in Desktop's importer, or every
    such call of a chat would be one line; the time the call started). A quote, a reaction and a
    1:1 notice name only us and the chat's contact (a quote by anybody else has an unknown
    author, such a reaction is dropped and such a notice skipped); a mention keeps the ACI it
    names, as live, with no name. Timer rules: a timer-change notice gets NO timer (Android
    never starts it; the core's notice rule would erase it at once); another notice keeps a
    timer only when the phone started it; an outgoing call line without a start is started at
    the import's time (the core would count it from the call's time); an incoming one keeps its
    after-read timer unstarted. The group call's line (`Mapper::group_call`) is built and
    unit-tested and waits for S4's groups. A ring of the moment of the export is reported unread
    and with no time left on its ring (a deadline at or before the import's time): the core
    keeps a ring by the time it has left, counted from the time the line was received, so a ring
    with a minute left would ring here when the call began shortly before the import or the
    phone's clock is ahead; with none left the core settles it at once as an unread missed call.
    Tests: `transfer/map/tests/content.rs` (one per mapping row and the caps) and
    `kourier-e2e/tests/signal_history_import.rs` `the_rich_archive_becomes_these_chats`,
    `a_reaction_from_the_phone_lands_on_an_imported_edited_message`.
    The offer is on by default (2026-10-11). A run can switch it off: the environment variable
    `KOURIER_HISTORY_TRANSFER=0` (exactly `0`, read where `kourier-app` makes the Signal
    network, in every build; the log then carries a warning; `cargo xtask smoke start
    --no-history-transfer`). It turns the offer and, with it, the descriptor's
    `HistorySource::PushedOnLink` off for that run. It is no setting and not meant for
    users: it serves the runs that are not about the history (a link that must not ask the
    phone) and, as a last resort, a transfer that goes wrong. The wiring is held by a test:
    `kourier-app` `app::transfer_switch_tests::a_normal_start_offers_the_history_transfer_by_default`
    runs `normal_setup_in` (what `normal_setup` does after the paths and the logging, over an
    environment it is given) on a private profile and asks the registry's Signal network for
    its descriptor; a `normal_setup` that registered `SignalNetwork::new()` fails it. (Debug
    builds only: it starts from the development keyring.)
    What roams, for direct chats and Note to Self (S4a): the first chat frame of a
    conversation gives archived, marked unread, the mute (converted by the Storage Service's own
    `storage::muted_until`; `i64::MAX` is for ever, a past mute stays past), the 1:1 timer in
    seconds with its version (at least 1, kept also without a timer) and the pinned order. Note
    to Self takes the same fields from its chat frame: the import keeps its mute and its timer
    when the frame has them. Only the Storage Service record of Note to Self lacks them (it has
    archived and marked unread only). A pinned chat is staged and shown even with no loud row
    or no row (Android and Desktop list it).
    Delivery puts the flags into the chat's upsert. It writes the timer into `peers` inside the
    chats zone before the features are computed, and only when its version is not below the
    stored one: the zone runs again after a redo or a lost stage, and the archive's old timer
    beside the version of a change the user made meanwhile would undo that change. The first
    features event of a chat carries the timer and its version when the timer is on. A version
    alone stays in `peers`, which every send reads. The pins are ONE `PinnedConversations` in a
    row of its own after every chat's upsert, sorted by `pinnedOrder`. The delivery holds that
    row until every row before it is settled. A chats row that the core refuses is appended
    again after whatever stands behind it, and a pin list that came first would make a bare
    chat of each key it names. The list leaves out a chat whose upsert was dropped for good: a
    row of chats that is taken back forgets the features of the chats it named
    (`peers.features_hash`, those chats only), and the list names only chats the backend's
    records say the core took. A chat known only by a number has no such record and stays named.
    At most 1,000 chats keep the pin (§13). The Storage Service sync that follows wins where it
    differs: a first record of a contact or Note to Self whose chat the core has states its
    flags even when they are false (D10). A first record of a contact the core has no chat for
    states nothing, or the sync would make a chat of every contact. Its pin list keeps the
    record's order across kinds and names a group only when the core has it (design §3 S4). On
    a fresh link the first sync runs before any group is fetched. So the first fetch of a group
    that the stored account record pins sends the list again, built by the same function
    (`storage::pins_with_new_group`, in the group's zone). That list is the record's, so it
    agrees with the sync's and with the archive's order (both are the phone's). The initial
    contact sync never lowers a stored timer version (Android's rule: a synced timer applies
    only when its version is higher), so it cannot undo what the import gave the core.
    Known limit: a pin of a person known only by a number is unpinned by the first Storage
    Service sync, which names only people with an account (such a chat is read-only).
    Known limit: a delivery row of chats that runs again (the core took it back, or a lost
    stage is read again) states the archive's archived, marked-unread and mute flags again. A
    flag the user changed on this device in the meantime comes back, for example a
    marked-unread dot that reading the chat cleared. This is rare. Known limit: a chats row
    that the core refuses while the delivery waits for the feeder is not yet settled when a
    lost stage is forgotten. Its chats can keep a features flag that says the core has them.
    Tests:
    `transfer/map/tests/frames.rs`, `transfer/deliver/tests/frames.rs`, `storage/tests.rs`,
    `contact_sync/tests.rs`, `kourier-signal/tests/history_import.rs`
    `direct_chats_take_the_archives_pins_archive_mute_marks_and_timer`,
    `the_pins_wait_for_a_row_of_chats_the_core_refused_and_for_its_singles`,
    `the_storage_sync_wins_over_the_archive_where_they_differ`, `tests/groups.rs`
    `a_pinned_group_stays_pinned_after_a_storage_sync`, and in `kourier-e2e`
    `the_list_after_an_import_has_the_phones_pins_archive_mute_and_marks` (also: a live message
    into a chat muted for ever is silent, one into a chat whose mute is over is announced),
    `a_message_sent_into_an_imported_chat_carries_the_archives_timer`.
    Groups (S4b): the group recipient keeps its master key and, from the phone's snapshot, the
    title, the members, invited and requesting people with their roles, the timer in seconds
    and our membership: member, invited (our ACI or our PNI among the pending, read from the
    service-id binary: 16 bytes an ACI, 17 with 0x01 a PNI), requesting (only among those who
    asked to join), left (a non-empty snapshot without us) or unknown (an empty snapshot,
    which Android writes when it had no state). The chat frame's timer (ms) wins over the
    snapshot's. A group we are invited to is shown, as a pinned chat is, with the request to
    accept it and the reason "Accept the invitation on your phone to join this group."
    (Kourier cannot accept an invitation); a request to join says that it "is waiting for an
    admin" (two new translatable `CannotSend` reasons, neither is "not a member"). The
    invitation, `selfInvitedToGroupUpdate`, is no notice (the live decoder makes none for our
    own invitation, and the contract has none), so such a chat has no row and lists after
    every chat that has one. The people of a staged group's lists are staged with it, so
    members have names, and in a group any person of the archive may write, react, be quoted
    or be named in a send state. Group changes follow the live decoder: an entry whose actor
    is absent is skipped (the contract's `by: None` means "you"), the item's author is never
    the actor, `Joined` for our own join, join by a link and accepted invitation (the live
    decoder says the same), a member whom an admin promoted from an invitation is added by
    that admin in both paths, our own leave is a notice of our own device, additions and
    removals by one actor are folded into one notice before the cap of 64, a title loses one
    outer pair of bidi isolates (known difference: a title with unbalanced bidi controls keeps
    the closers Android adds, which the live notice does not show). The viewmodel says "X
    joined the group" for `MembersAdded { by: X, users: [X] }` and "X removed the group name"
    for an empty title (the live decoder makes an empty one only for an action without a blob
    or a blob that decrypts to nothing, as Desktop reads it; a blob that does not decrypt makes
    no title notice). Group calls are lines of their group. Delivery writes the `groups` row
    in the chats zone WITHOUT the snapshot, with the flags the Storage Service merge compares,
    the membership the archive says ('pending' for an invitation, 'requesting' for a request
    to join, 'left'; what the server said before stands over it) and the hash of the features
    sent ("still loading", a new translatable `CannotSend::StillLoading`, until the first
    fetch for a member or an empty snapshot; the invitation and the request to join as above;
    "not a member" when left or gone). It queues a refresh for every imported group, a left
    one too (D10), and wakes the job runner. A row whose server state is there already (a
    refresh ran during the delivery, or after a rebuild) gets that state's title, members and
    timer and never the archive's, so that a rebuilt core gets a whole group. The first fetch
    is a "first" one: the server's title, members and timer replace the archive's and no
    notice is made for what changed since the export (Desktop shows those changes: a known
    difference), and it states `request_pending` from the server's state. A first answer that
    is no state ends an invitation: a 403 or a 404 states `request_pending` false (Desktop:
    its 403 on the state falls back to a pre-join fetch that fails the same way, and the group
    is left; the same on the live path when an invited row's state is refused), while a 403 on
    a request to join changes nothing (the server gives a requester no state and the request
    may still stand; Desktop keeps it pending too). A 401 of the group server is a refused
    credential: the job backs off and asks again with fresh credentials, for at most 30
    attempts (about four hours), after which the next start queues the refresh again. Every
    start queues a refresh for an imported group that was never fetched and is not left or
    gone. `groups.membership` knows 'requesting' (backend.db schema 20, migration 0020, which
    also drops the dead `deliver_hinted`). `stage::FORMAT` is 5. Tests:
    `transfer/map/tests/groups.rs`, `transfer/deliver/tests/groups.rs`, `groups/tests.rs`,
    `kourier-signal/tests/history_import.rs`
    `an_imported_group_comes_from_the_archive_and_its_first_fetch_replaces_title_members_and_timer`,
    `a_left_group_is_refreshed_once_and_group_items_notices_and_calls_are_imported`,
    `nothing_of_the_archives_groups_reaches_backend_db_before_the_commit`,
    `the_storage_sync_wins_over_the_archives_group_flags_and_is_silent_where_they_agree`, the
    kill at every zone with a pinned group, `tests/history_transfer_attack.rs` (a group title
    with the marker; the master key and the invite link's password, a needle of its own, in no
    file and no page of `backend.db` or of the stage), and in `kourier-e2e`
    `an_imported_group_is_listed_from_the_archive_and_a_send_reaches_the_servers_members` and
    `an_imported_invitation_is_offered_with_its_reason_until_the_first_fetch_finds_it_over`.
    Known limits: (a) a notice title keeps the closers Android adds for unbalanced bidi
    controls (safer; a cosmetic difference from the live notice); (b) a take-back after the
    group's refresh, followed by a single dropped for good, leaves the group's features flag
    cleared although the core has the group from the refresh (its pin and the Storage sync's
    first flags wait for the next fetch); (c) a request to join never learns that it was
    declined (the server gives a requester nothing).
    **Attachment pointers (S5, `stage::FORMAT` 6; user decisions of 2026-10-10: old media
    "Ask the phone" (step S6), long texts "Fetch automatically", wording "Two texts").** A
    message's attachments (at most 1,000 per message, the cut counted), the rest of a long
    text and a sticker's image become `Attachment`s with the live path's bounds (file name
    255 bytes, blurhash 128, content type 255, waveform 100 bars, duration, dimensions,
    caption) and their kind from the archive's flag ENUM (`VOICE_MESSAGE` 1, `GIF` 3; the
    wire's GIF bit is 8: `receive::media::archive_kind`); every voice-flagged attachment is a
    voice note, as live. Names: the client UUID, the digest or the plaintext hash
    (`plaintext:<hex>`, which delete-for-me syncs now carry both ways). One whose transit
    place is valid (a CDN key `valid_cdn_key` takes, a CDN number 0, 2 or 3, a key of 64
    bytes, a plaintext hash or digest of 32 bytes, any size: 0 is an empty file, legal as live
    and in Desktop), within the receive limit and young is `Remote` with `remote` =
    `BackendBlob` v 3, the prost-encoded backup `FilePointer` trimmed to content type,
    dimensions and the transit locator (with the size; no `localKey`, no media tier); `Pointer::from_blob` reads it back and the download checks the plaintext hash
    (else the digest). Every other one is `AttachmentState::Unavailable`, shown as "Not
    available on this device", with NO pointer and no key but with its names and its
    message's id (author and original sent time), what step S6 uses to ask the phone for it
    (§6.11 "Asking the phone", built 2026-10-11);
    "No longer available" (`Expired`) stays the state after the server said so on a click
    (404, 410, a 403 of CDN 0). Each kind is counted (`attachment_not_available`,
    `attachment_too_old`, `attachment_too_large`, `attachments_cut`). The age rule is Signal
    Desktop's (`AttachmentDownloadManager`, which uses it only to pick what it fetches by
    itself): the upload time, else the message's original sent time, is old when it is at
    or before the reference minus twice max(the remote config's message queue time, 30
    days); the reference is the phone's export time (`ArchiveInfo.backup_time_ms`) clamped
    to within a day of the import's time, so this computer's clock decides nothing alone and
    a re-read gives the same rows; the queue time and the receive limit are read from
    `backend.db` when the read begins (none stored: the 30-day floor, a 60-day window, and the
    default limit; the log line "the import judges the age of attachments" says which; a
    database error there fails the read as an internal error and never gives the floors). The
    import starts no download: a history attachment waits for a click (invariant I13), except
    the rest of a young long text, which the core fetches by itself and joins into the body
    (into the NEWEST revision of an imported edited message, whose revision 0 has no body),
    and not for a message that is due or hidden by the expiry policy (the refusal a click
    meets too); an old long text keeps its first 2 KiB. A history attachment the core finds
    above the receive limit the account states (the import judged by an older one) becomes
    "not available on this device" with its pointer dropped, and a LIVE pointer to a CDN
    this build has no base URL for is "not available" at once, not a "Retry Download". A quote's thumbnail, a view-once pointer and a
    contact card's avatar never become a pointer. Tests: `transfer/map/tests/media.rs` (every
    locator shape, the age bound and its reference, the kinds, long texts, stickers, the cap),
    `attachment.rs` (both blob formats, a download checked by the plaintext hash, 410 and the
    403 of CDN 0), `kourier-core` `attachments/derive.rs` and `policy.rs`,
    `kourier-signal/tests/history_import.rs`
    `an_imported_picture_downloads_on_a_click_and_the_import_asks_the_cdn_nothing`,
    `the_servers_queue_time_and_receive_limit_decide_at_the_import` and
    `the_age_window_counts_from_the_export_time_when_it_is_within_a_day_of_the_import`,
    `kourier-core/tests/attachments.rs` (a due or hidden message fetches no long text by
    itself, an imported file above the limit is not available), `delete_for_me.rs` (the names
    of a delete tried in order over all attachments), and in `kourier-e2e`
    `an_import_fetches_its_young_long_texts_and_nothing_else_by_itself` (the mock CDN's
    request log holds only the archive and the young long texts until a click). The fixture
    page `unavailable` shows every state (pictures: `cargo xtask shots --page unavailable`;
    the fixture account can ask its phone, so the gone ones offer Download and the ones
    being asked for or finally refused do not); a gone sticker is a small framed box with the
    note and its own Download inside, and on a picture the note is a few words (the whole
    sentence is its tooltip) that a small tile may still cut to its width. Known
    limits: (a) an edit or a forward of an imported own message whose attachment was never
    downloaded needs the file (only wire pointers are re-sent) and fails; (b) an own message
    the phone was still sending at the export has empty locators and stays "not available"
    although the phone's later transcript has the pointers; (c) the queue time and the
    receive limit are those stored at the read, and the reference is the export time only
    within a day of the import's time: a re-read of a lost stage after a change of the remote
    config, or more than a day after the export, can judge differently for the rows not yet
    delivered (delivered rows stay: ingest is idempotent); (d) an old sticker is "not
    available" although its pack could give it (a later step); (e) accepting a message
    request, or unblocking a chat, long after the import fetches the long texts of that
    chat's imported messages (the core's `fetch_long_texts` does not look at the age rule), and
    those the server no longer has end as "No longer available". Asking the phone for what is
    not available is built (S6, §6.11 "Asking the phone"), and the offer is on by default
    (2026-10-11).
    The screens (2026-10-09; built and tested on the fake network, and reachable with
    Signal since the offer is on by default and its descriptor then says
    `HistorySource::PushedOnLink`; a run with `KOURIER_HISTORY_TRANSFER=0` has neither):
    - One view of a running transfer (`kourier_viewmodel::transfer`), shown in the
      account banner (since link feedback L3 no longer on the link page: the page
      closes as soon as the core has the account, at the transfer's first report or
      the account's first state, and the chats are usable meanwhile):
      "Waiting for your phone to prepare the message history…" (the phone prepares and
      uploads the history: on Signal its user agreed at the scan, before the link, so
      nothing is asked any more) with "Continue Without History"; "Downloading message
      history…" with a bar and "12.3 MB of 45.6 MB" (only the bytes while the size is
      unknown; a count that steps back is a shorter bar) and the same button; "Importing
      history: 12,400 of 50,000 messages. New messages arrive afterwards." with a
      determinate bar from the backend's ephemeral `BackendEvent::AccountProgress`
      (`UiEvent::AccountProgress`, not stored), and no button; the line keeps its reason
      because nothing new arrives until the import is delivered (a message sent meanwhile goes
      out at once since L6, §4.5). After it (or without a transfer) the same reports drive "Updating contacts
      and groups: 120 profiles, 8 groups left" until the backend says `finished`. The
      login's `Done` still comes after the transfer's end, but no page waits for it; after
      a restart the banner shows the same view. An empty list says why it is empty (and
      a search that matches nothing says so before any of that); "Clear Chat History…"
      and "Delete Chat…" wait (disabled, with the reason: "Available once this chat's
      history has arrived") while a chat still fills or its account's transfer runs (the
      core itself does not refuse a delete in that time, a phone's delete of a chat can come
      then too: the rows of a chat deleted while its history is delivered are dropped, they
      neither bring the chat back nor fail the delivery, `ingest/message.rs`
      `in_deleted_chat`; a LIVE message still lists a deleted chat again);
      closing the window while the transfer or the update runs asks "Quit While …?" (the
      work goes on at the next start; the tray's Quit and the end of the desktop session
      never ask).
    - A logout in the middle ends the transfer without an event (the backend deletes it
      with the login): the UI drops a running state when it sees the account logged out,
      and "Link Again" shows the steps of that login, never a transfer.
    - The end. Done: one passing notice, and the UI dismisses. Skipped: nothing when
      this UI sent the skip, otherwise one notice that the history was not
      transferred; dismissed either way. Failed: a banner that stays, also across
      restarts, with one sentence per kind, "Dismiss", and "Try Again…" for every kind
      but `UpdateApp`. A notice is shown once per end; a dismissal is sent for every
      report of an end that was shown, because a dismissal can cross a report.
    - Banners. One account shows one: its state or its transfer, by the order
      connecting < getting new messages < TRANSFER < offline < TRANSFER FAILED < stopped <
      restricted < clock < challenge < logged out. The transfer's banner is chosen apart
      from the state's banner, so with several accounts both can show (the state above,
      the transfer under it) and the state of one account never hides the failed
      transfer or the skip of another; among transfers a failed one goes first.
    - "Transfer History from Phone…" (Settings, group "Accounts") for every account of a
      network whose descriptor says `HistorySource::PushedOnLink`: the question "Remove
      and Link Again?" (the default is Cancel; it says what is removed, that what
      exists only on this computer is lost, that older attachments may not be
      transferred, that the phone is needed, and that the account stays removed when
      linking is cancelled afterwards), then `RemoveAccount` and `AddAccount` for the same
      network, and the link page. "Try Again…" of a failed transfer asks the same
      question. Only the confirmed question removes an account, once; its confirming
      button acts only after the question has been open for a second (it opens under
      the pointer of a double click), and its two buttons stack when the dialog is too
      narrow for both.
    - The fake network walks a transfer (`TransferScript`), obeys skip and dismissal and
      reports the stored state at every start and connect; fixture pages
      `transferwait`, `transferdownload`, `transferread`, `transferupdate`,
      `transferbanner`, `transferfailed`, `transferask`;
      `--fixture-link --fixture-transfer=<…>`.
    Left: the passing notice is lost when no window exists at the end (hidden in the
    tray and released); a skip sent before the core has the account is dropped (the
    button stays); the end of our own skip is announced as "not transferred" when
    Kourier was restarted before the backend reported it; an account picked in the
    drawer hides the other accounts' banners, the transfer's too; the action shows for
    Signal only once its descriptor says `PushedOnLink`.
15. Calls are shown, not taken (user, 2026-10-08: "We should just show it in chat in ring
    state and then - as post stete"; a group call starting: "as i menationed before"). A
    line in the chat while a call rings, which becomes what the call became. No calling.
    First in the order he chose ("Calls first, then history"). Built (2026-10-09), on the
    fake network: one row per call whose body is `SystemEvent::Call(CallLine)`; a backend
    reports each signal as a message with that body (`IncomingMessage::call`), and the
    core merges it into the row by a join that only moves a line up its states (ringing <
    missed or unanswered < declined < answered; "started" for a group call nobody rang
    us for; a delete removes it), so any order of signals ends in the same line. A line
    is found by (conversation, call id) on every network, because the other side chooses
    the id; the other side may only say started, ringing or missed, the user's own
    devices say the rest; a ring's deadline is the time left at receipt, capped at two
    minutes, on the local clock, kept in core.db (schema 11) and swept by a task of its
    own across restarts and suspends; a missed incoming call counts as unread and
    notifies like a message, and both are taken back when a later signal shows it was
    answered; a ring that ran out while Kourier was closed goes into the catch-up
    summary; a line takes the chat's disappearing timer; a line of an imported history takes
    the timer and the read state the import states (a ring does not keep its `read`); nothing
    of a line is ever sent.
    What the wire does not give and Kourier therefore does not show: how long a call
    lasted or that it ended, a ring state for calls made from the phone, the end of a
    group call. Kourier never replies to a call (a reply from one of the user's devices
    can end it or stop the phone ringing). Known limits: a caller who reuses the id of
    an old call of the same chat joins the old line; a peer signal can still change the
    kind and the shown time of a settled line; missed calls held for the summary are
    kept in memory only; group lines are in no picture. The Signal backend's side
    (2026-10-09, `kourier-signal/src/receive/calls.rs`, tested against the mock server,
    which must have received nothing from Kourier after each signal): a 1:1 Offer rings
    for 60 s (RingRTC's ring time) minus the envelope's age on the server's clock at both
    ends (stage 1 keeps the delivery time now, backend.db schema 15); the caller's Hangup
    is a missed call only when the caller gave up (type NORMAL); a group ring is RingRTC's
    ring intention in the opaque payload, read before an Offer beside it; a
    `groupCallUpdate` is a "started" line keyed by the ring id its era id maps to, no
    longer a bubble per join and leave; the phone's `CallEvent` sync settles a line as
    answered, declined, outgoing or unanswered, or deletes it, and opens no chat the core
    does not have; a caller whose chat is not accepted gets a silent missed line with no
    ringing phase. One exception to "never send" is accepted and documented: an envelope
    that does not decrypt gets the usual retry request to its sender whatever it held.
    Ignored: the phone's call-log syncs (clearing the call log on the phone does not
    remove lines) and call links. Old "Call" placeholder rows of earlier builds stay as
    they are. STILL TO DO: one real call on the test account, which alone can show
    whether an Offer and the caller's Hangup reach a device that never rings, and how an
    iPhone behaves (everything read about the phone is Android).
16. Media a phone can show (user: "Video should be supported as well. Playing video should
    support minimum + what system can. In case of sending vebm and mkv we should ask user
    about conversion or not."; "Pictures - also should check compatibility and if not - ask
    yoser to convert"). Conversion uses the system's ffmpeg when it is installed ("System
    ffmpeg if installed"); without it only "send as it is".
17. The other message kinds are shown ("Yes, other types should be shown and there should
    be a button to download. same as other attachents - should be a button to download"),
    read-only ("Read-only content": a poll's question, options and counts; a contact card
    with "Save as vCard"; payments, badges and pins as notices). View-once media is shown
    once and then removed, with no download ("Show once, then remove").
18. A message can be saved as Markdown from its context menu ("For messages there should
    aslo be button to download (as markdown) in context menu").

**The first publication is archives on GitHub, nothing else** (user, 2026-10-08: "At first
we should publish only in archives on github"). So the order of the list above changes:
item 8 (Flatpak) and the Flatpak parts of items 10 and 12 move behind the first release,
with Flathub, AUR and the other channels of §14.3. What the first release needs instead
is small: `cargo xtask dist` makes, from a clean commit, a binary archive of the installed
tree (`bin/`, `share/` with the desktop file, icon, metainfo, catalogs, the licence and the
third-party notices) and a source archive, with `SHA256SUMS`; the maintainer builds them on
his own machine and attaches them to a draft GitHub release, when he asks for it. A binary
archive is not portable the way a Flatpak is: it runs where the system has the Qt and KDE
Frameworks it was built against or newer (the archive says which), so the source archive
with build instructions is the answer for everything else, and an AppImage is the later
option if a portable single file is wanted before Flatpak. Still before any first push:
item 1 (name, icon, application id) and the `TODO(maintainer)` blanks. Decided by the user
on 2026-10-08 for the publication:
- The working documents that name private paths are hidden from the public tree ("hide
  it"), and "git history (if exists) should also be free from private info": the public
  repository does not get this repository's history (it holds the old application id, the
  author's address and private paths in old versions of the documents). It starts with a
  history of its own, made from the exported tree of a release; this working repository is
  not rewritten.
- The test network ("It should stay in code but disabled in compile time for relase by
  default. But we need that staff for future tests"): it stays the cargo feature `fake`,
  on in development builds and tests, off in what `cargo xtask dist` builds.
- Signal links (`sgnl://`, `signalcaptcha://`) are not claimed by the archive's desktop
  file ("Ok. We also can add some buttons in settings to help with it or provide some
  scripts"): a setting and a script that switch the claim on and off for the user.
- The dependencies' sources are not attached as an archive ("May be we can provide some
  sort of script to fetch references, but not all archive"): the source archive carries a
  script that fetches exactly the locked dependency sources.

As built so far (2026-10-08, the first batch of the gate; the front-end contract it must
keep is §8.10):

- Item 3, theming (QML only). Text colours are made readable on what they stand on:
  `Style.readable(c, bg)` moves a colour's lightness until it has 4.5:1 against `bg`,
  `Style.secondary(text, bg)` is the quiet text colour (never the disabled-text role), and
  text or glyphs on accent fills (unread pill, selection, file action disc, step numbers) are
  the theme's own pairing, `highlightedTextColor` on `highlightColor`: white on Breeze's blue.
  (User decision, 2026-10-08: "Back to white". The pass had first computed a readable colour
  there, dark grey in Breeze, because white on Breeze blue is 2.5:1; the theme's pairing wins,
  and a high-contrast scheme brings its own.) Own bubbles are the page
  background tinted by 20 % of the accent with normal text. Frames (bubbles, reaction chips,
  date pill, account chip, QR card, icon tiles) are `Style.frame(background, text,
  Kirigami.Theme.frameContrast)`: faint in Breeze, solid in high-contrast schemes (every
  bubble has one now; the user: "Ok. Looks good"). Overlays on pictures and the image viewer use
  the Complementary colour set; the only literal colours left in QML are the black-on-white
  QR card and the 0/1 fallback inside `Style.readable`. Labels that set a weight, italic or
  underline are bound as `font: Style.styled(Kirigami.Theme.defaultFont, {…})`; the pairing
  code uses `fixedWidthFont`. Heights of rows, pills, chips and the page header follow their
  content and never go below the old size. The application's logo is `Style.appIcon`: the
  theme's icon named as the application id when the theme has one (`QIcon::hasThemeIcon`,
  asked once), else the bundled file; the About page gets the bundled file as
  `programLogo` in the same case. `cargo xtask shots --matrix` (third batch) renders
  `list`, `chat`, `compose`, `settings` and `readmore`, wide and narrow, under seven other
  looks into `target/shots-matrix/<variant>/`: Breeze Classic, a light and a dark
  high-contrast scheme (written here, `xtask/assets/`; KDE's own is not installed), a
  14 pt font, right-to-left (a Hebrew locale, so the digits stay western; the texts stay
  English), no KDE platform theme, and that with the Fusion style. It is for looking: not
  compared, not part of `ci`. What the first look found, and what became of it (fixed the
  same day; the Breeze pictures did not change except for the avatar initials):
  - right-to-left: the timeline was not mirrored (own bubbles stayed right while the
    inside of a bubble mirrored). Anchors and positioners mirror by themselves, an explicit
    `x` does not: `MessageDelegate.qml` places the avatar, the column and the hover toolbar
    by `atRight` (`own !== mirrored`), as it does the paddings of the reaction chips and
    the tight corners, which are physical sides too. Own bubbles are at the left now, the
    others with their avatar at the right. The composer's file list had collapsed: its
    `gutter` had no anchor and the mirrored anchors of the row hung on an item at x = 0;
    it is anchored. Left as it is: the "Name:" prefix of a group preview stands at the
    right end of the line, which is the start of a right-to-left line (it looked wrong only
    because the fixture's text is English); the album grid is not mirrored (a design
    question); the emoji popup's `x` is not mirrored and no picture shows it.
  - without the KDE theme the "edited" label was larger than the time beside it: a theme
    font assigned whole is not always shown at the size it names (`smallFont` says 12 pt
    there and a label shows 9). The label now takes the font of the time label itself.
  - high contrast: one switch, `Style.highContrast` (`frameContrast >= 0.8`; Breeze has
    0.2), and behind it the selected conversation row and the rail's active account get a
    solid frame, quiet text (`Style.secondary`) is the plain text colour, and the groups of
    the settings page get a frame laid over Kirigami Add-ons' card (which has no frame
    property). Not done: the separators inside those cards and the About page.
  - the initials of avatars without a picture used pale palette hues as they were (under
    2:1 on white in every light scheme): they go through `Style.readable` now, the one
    change to the normal pictures (44 light ones and the two `timer` dark ones).
  - 14 pt: nothing is clipped; the small icons keep their size beside the larger text.
  Not done: standard icon names, the automatic contrast-ratio check of the pictures, Oxygen,
  odd accent colours, the Union style and a no-animation run.
- Item 4, KDE guidelines. Title Case for buttons, menu entries, titles and page names;
  sentence case for labels, tooltips, messages and the inline "Read more (N words)" link
  (the user's wording), in QML and in the strings made in Rust. No disabled stubs ("New chat" is gone until the
  feature exists). Shortcuts are `Shortcut` objects in `MainWindow.qml`, each with the
  platform's `StandardKey` where Qt has one and a fixed key beside it: Ctrl+K and Find focus
  the conversation search (Enter opens the first match); Alt+Down, Ctrl+PgDown and Ctrl+Tab
  open the next chat, Alt+Up, Ctrl+PgUp and Ctrl+Shift+Tab the one above
  (`App.adjacentConversation`; no wrap); Preferences or Ctrl+, opens the settings; Quit or
  Ctrl+Q quits; Close or Ctrl+W follows the close button (with the tray off that quits).
  They stand aside while a popup has the keyboard. The conversation list is one tab stop
  with arrow keys, Enter and Space, a focus ring and accessible names; in a wide window Tab
  walks search, conversations, timeline, composer. `tests/shortcuts_ui.rs` drives the keys
  and audits that every visible button and field of the main window and the settings has an
  accessible name. The tray is off by default (`Prefs::default().tray`); a saved choice is
  kept; `cargo xtask perf` writes `tray = true` into its own profile. "Mark as Read" for
  a chat that is not open (third batch): `UiSource::mark_read(conv)` sends
  `UiCommand::MarkRead` whatever the focus and whether or not it was sent before (the core
  does the receipts, the read sync and the timers); `App.markRead`; the conversation list
  has a context menu with that one entry, opened by right click, long press, the Menu key
  and Shift+F10, and only for a row with unread messages or the dot of a chat marked
  unread on another device (a menu with nothing in it does not open). It has no shortcut:
  chat applications have no convention for one. Reading a chat, by opening it or by the
  entry, clears that dot too (§4.8). Open: long press is not tested (no touch device
  offscreen); the fixture has no chat that is only marked unread, so the entry for one is
  tested in the core, not in the window. Not done: a Keyboard Shortcuts page, the manual
  Orca pass.
- Items 5 and 6 without a remote. `LICENSE` (AGPL-3.0-only, the FSF text), `LICENSES/`,
  `REUSE.toml` (the tree annotated by path, no per-file headers; `reuse lint` passes on all
  files), `README.md`, `CONTRIBUTING.md` (DCO), `CODE_OF_CONDUCT.md` (Contributor Covenant
  2.1), `SECURITY.md`, `AI-USE.md`, `GOVERNANCE.md`, `PRIVACY.md`, `ACCESSIBILITY.md`, a
  `CHANGELOG.md` skeleton; every blank the maintainer must fill is `TODO(maintainer): …`
  (`grep -rn "TODO(maintainer)" .`), and none may remain at the first public push. `.github/`
  has issue forms, a pull request template and `workflows/ci.yml` with the light jobs (fmt,
  REUSE, cargo-deny, clippy on manual dispatch), written and never run; a full `cargo xtask
  ci` on a hosted runner needs more disk than its 14 GB, Qt 6.11 with KF6 (a container image
  or the Flatpak SDK), Node 24 and a D-Bus session. `deny.toml` (allowed licences, `openssl*`
  banned, crates.io plus five git sources), `about.toml` + `about.hbs` (third-party notices:
  `cargo about generate --locked --all-features about.hbs -o THIRD-PARTY.html`, generated,
  not committed) and `cliff.toml`. Local results with no dependency changed: `cargo deny
  check` passes with one ignored advisory (RUSTSEC-2025-0141, bincode 1.3.3 unmaintained,
  through libsignal's zkgroup; look again at every libsignal upgrade) and 31
  duplicate-version warnings; `cargo audit` finds no vulnerability. Open from these items:
  Renovate, an SBOM, Scorecard and CodeQL workflows, fuzz targets, per-file SPDX headers;
  and before any first push: `CLAUDE.md` and this document name private paths and working
  roles, the commit author is the user's own name and address, and the git history holds
  the old application id. The network icons in `crates/kourier-qt/icons/` (Signal, WhatsApp,
  Threema circles in brand colours) are close to the logos and have no recorded origin:
  they go with the icon redesign of item 1.
- Item 2, the KDE shim (second batch, 2026-10-08; the KF6 development packages are
  installed). The KDE pieces are in `crates/kourier-qt` only: `cpp/kde.cpp` and
  `cpp/i18n.cpp` (bridge `src/kde_ffi.rs`, Rust side `src/kde.rs` and `src/l10n.rs`).
  `build.rs` links KF6CoreAddons, KF6I18n, KF6I18nQml, KF6Crash and Kirigami Add-ons'
  KirigamiApp through cxx-qt-build; KDE ships CMake config files and mostly no pkg-config
  files, so the include directories and library names are written down in `KDE_LIBRARIES`,
  and a missing header stops the build and names the package (prefix `/usr` or
  `KOURIER_KDE_PREFIX`). `prepare_environment()` is gone and the process no longer sets
  `QT_QUICK_CONTROLS_STYLE` (an explicit value still wins: the screenshot step and
  `scroll_ui` set it). `run()` does, in this order: `l10n::install()` (the application
  domain and the `Translator`), the application object, `kde::set_about_data(about::info(),
  APP_ID)`, `kde::apply_defaults(APP_ID)` (`KirigamiAppDefaults::apply`: the style when
  nothing chose one, Breeze as the fallback icon theme, the desktop's colour scheme; then
  the window icon by its theme name), `kde::start_crash_handler()` in a normal run only,
  and `l10n::setup_engine()` before the engine loads QML. The About data is set BEFORE
  `apply` on purpose: Kirigami Add-ons connect to the application data and start KCrash
  themselves when it changes after `apply`. A normal run is a real profile without
  `--fixture`, `--screenshot`, `--dev-control` and not a perf build; no other run starts
  KCrash. `main` installs the panic hook with `kourier_qt::kde::panic_sink`, which gives
  KCrash the redacted line. Settings has "About Kourier" (Kirigami Add-ons'
  `FormCard.AboutPage`); `App.version` comes from `about::info()`. There is no "About KDE"
  row (user, 2026-10-08: "If its ok to have it without beeing part of KDE ecosystem - keep.
  But I think its too early."; `FormCard.AboutKDEPage` is one row away when it is wanted).
  `tests/kde.rs` proves the plumbing with a catalog it writes itself. Open: `KAboutLicense`
  has no AGPL entry, so the page names the licence "Custom" with the SPDX line as its text;
  "Libraries in use" lists only what Kirigami Add-ons add themselves, because
  `about::info().components` is empty (libsignal and the rest belong there); the fixture
  page `about` shows the page; start-up time and memory with the new libraries
  were measured on 2026-10-08 (`cargo xtask perf --quick`, 3 starts): cold start 379 ms
  (worst 396) of 400, with files out of the page cache 510 of 600, idle memory with the
  window shown 88.9 MB anonymous of 100 and 130 MB PSS of 150, hidden 60.1 of 70 and 101
  of 110. The KDE libraries cost little: loading them under 2 ms, `KirigamiAppDefaults`
  1 ms, all translated strings through KI18n under 1 ms. A first measurement had missed
  the start budget with 416 ms: `QIcon::hasThemeIcon(APP_ID)`, asked for the About logo,
  and the themed window icon are lookups of a name the theme lacks in a run that is not
  installed, which walk every directory of the theme and its parents (37 ms).
  `kde::app_icon_in_theme` now looks for the file an install puts into the fallback theme
  (`icons/hicolor/scalable/apps/<id>.svg` in the data directories), and the themed window
  icon is set only when that exists. Known places to win more, not taken: the KDE desktop
  style's `TextField` measures the glyph U+2588 for every field, Noto Sans lacks it and
  fontconfig searches the fallback fonts (about 14 ms through the search field of the
  first frame); the list's context menu is made with the page (about 3 ms); the core's
  `pending_derives` reads the whole attachment table on a blocking thread at every start.
  KCrash in a normal run was not exercised;
  a release build is non-dumpable, so DrKonqi gets the panic line but no backtrace (kept:
  the keys in memory matter more). The window icon is the theme's icon of the application;
  a run that is not installed has none there and gets the bundled file once the first
  frame is on screen (`kde::set_fallback_window_icon` in `App::ready`; user: "leave it as
  is. Or add icon after if it possible"), so the start does not load the SVG icon plugin.
- Item 11, translations (third batch, 2026-10-08): the pipeline and a first language.
  Every user-visible string made in Rust has a context and `%N` placeholders, the catalog
  `po/kourier.pot` (562 messages) is extracted and kept current by CI, the pseudo-language
  proves the chain on the real application, `TRANSLATING.md` is written, and Ukrainian is
  drafted in full (the user's review is open): §9.5. Open: the desktop file and metainfo
  translated at install, the translation platform (it needs a public repository).
- Release archives (2026-10-08; `cargo xtask dist`, `xtask/src/dist.rs`, `RELEASING.md`;
  nothing is pushed or published by it). A binary archive of the installed tree (binary,
  desktop file without link handlers, icon, metainfo, catalogs, `LICENSE`,
  `THIRD-PARTY.html` with the crates by `cargo about` and a section for what is linked in
  and is no crate: SQLCipher, SQLite, BoringSSL, the Rust standard library; a generated
  `README.txt`), a source archive (`git archive` of the commit the run started with) and
  `SHA256SUMS`; the same input packs to the same bytes. The release binary is built in its
  own target directory with `--no-default-features --features signal`, so it has no test
  network, and with every private path mapped away (`--remap-path-prefix` and
  `-ffile-prefix-map` for the workspace, the cargo home, the build directory and the
  toolchain): a plain build names the maintainer's home directory 1,331 times, this one
  not once. A stamp beside the binary (commit, features, Qt, KDE Frameworks and rustc
  versions, checksum) is written after the build and read before packing, and the commit
  and the working tree are compared before and after the build. The names carry a grade:
  the final names only for a `--release` run that found nothing in `NOT-READY.txt`
  (`TODO(maintainer)` blanks, the placeholder id, tracked files naming private paths, no
  repository address), `-unreleased` for any other run from a clean tree, `-dirty` for a
  tree with changes. After packing the archive is unpacked and checked: the exact set of
  paths, modes and owner, `--version`, `--fixture` refused, no measuring build, no link
  scheme in the desktop file, the notices, the validators, and that the source archive is
  the commit. `README.txt` states what the binary needs as read from it: glibc 2.38, Qt
  6.11 (it uses three symbols of Qt's private API, which hold for one minor version), the
  KDE Frameworks it was built with, and what it cannot show (the org.kde.desktop style, a
  Secret Service provider). An independent review found twelve defects in the first
  version (final names without the gate, old archives left by a refused run, missing
  notices, a binary not tied to its commit, the test network in the binary among them);
  all are fixed and were checked again on real archives. Not covered: a reproducible
  build (only the packing is repeatable), signatures (only the tag is signed), the
  dependencies' sources (a `cargo vendor` archive is the maintainer's decision), a start
  of the application from the archive on another system.
- A panic is logged as one line made by `kourier_app::process::install_panic_hook`:
  `panicked at <file>:<line>:<column> on thread "<name>"`. The panic's message is never
  part of it (`unwrap`, `expect` and `assert_eq!` print the values they failed on); a
  dependency's file is shown without the registry path; the line passes `logging::redact`,
  goes to the log and to the front-end's sink, and then the previous hook runs.
- Avatar shape (user, 2026-10-08: "add option to force disable avatars rounding. May be
  this should also be theme-dependent"). Settings > General > "Round avatars"
  (`round-avatars` in `kourier.toml`, default on, a shared preference: §8.10 R7). Off,
  avatars are squares whose corners have the theme's radius (`Kirigami.Units.cornerRadius`),
  so the shape follows the theme; everywhere the UI draws an avatar and in the notification
  avatars. Avatars are cut in the image provider, not clipped in QML: the request is
  `image://kourier/avatar/<handle>?v=<rev>&r=<radius>` with the corner in pixels of the
  requested size (a circle at half the side or more, or when `r` is missing), and the
  payload header is `[format, flags, codec, radius u16 LE]`; a change of the setting is a
  new request, so the avatars on screen change at once. `Avatar.qml` draws the tinted
  shape, the initials and the glyph itself, because Kirigami Add-ons' `Avatar` draws a
  fixed circle outline. The notification avatar takes the theme's corner as a share of the
  side from QML (`App.settings.avatarCorner`; Breeze's share until the window has said).
- Two CI tiers (user, 2026-10-08: "tests should be optimized or there should be some lower
  count subset to test regularly and full set for bigger cases"). `cargo xtask ci --quick
  [--since <rev>]` (`xtask/src/quick.rs`): rustfmt, clippy, deps-check, the unit tests of
  every package, and the integration tests of the changed packages and of those that depend
  on them (from `git status`, the package graph of `cargo metadata`); qmllint only when a
  `.qml` file changed; no screenshots. A changed root `Cargo.toml`, `Cargo.lock`,
  `rust-toolchain.toml` or `.cargo/` makes it the full run. First real run: 4 min 16 s for
  a batch that touched five packages (901 tests), against 9 to 14 minutes. Since T1
  (2026-10-13) both tiers run their tests through cargo-nextest (§10.5): for a change in
  `kourier-viewmodel` (reached: itself, `kourier-qt`, `kourier`) the test steps took 5.0 minutes
  through `cargo test` (unit tests 44 s, `kourier` 26 s, `kourier-qt` 233 s) and 2.1 through
  nextest (23 s, 17 s, 87 s), same tree and machine. Still open from the
  audit of waiting times: the rest of its list.

### 14.3 Soon after R1

Signed self-hosted Flatpak repo with update delivery (libsignal bumps every 2-4 weeks); AUR
(`<name>`, `<name>-git`) and `flake.nix`; Flathub submission by the user (hand-written manifest, AI
disclosure, talk-name justifications, domain verification) once several tagged releases exist; in-app
color scheme picker (KColorSchemeManager); `ConfigurationView` settings modules (spell checking,
shortcuts, about); conversation details as a context drawer or third column; `Accessible.announce()` for
incoming messages; notifyrc + "Configure Notifications…" + "Mark as Read" action; `DBusActivatable` with
a D-Bus service file; opt-in KRunner runner; a documentation site (user guide, FAQ, threat model).

Asked for by the user, not scheduled yet (2026-10-10, his words: "add screenshot mode for
selected messages for export"): a mode in which the user selects messages in a chat and
exports them as a picture. His answers: the bubbles exactly as Kourier draws them, in the
current theme, as one tall PNG; whether other people's names and avatars are shown is the
exporting user's choice, among the options of the export ("Also export can have options
for user"). Kourier has no selection of several messages today; this mode brings it, and
"Save as Markdown" of several messages could use the same selection.

### 14.4 Later

OBS deb/rpm repositories, Snap (`kde-neon-6`), AppImage, a nixpkgs PR; Windows (Craft, MSIX/winget,
code signing) and macOS (Craft, dmg, notarization, Homebrew); GlobalShortcuts portal; Purpose share
plugin; configurable shortcuts and a command bar; touch/mobile pass; selenium-webdriver-at-spi UI tests;
Flathub reproducibility checker; KDE incubation if the license and build questions resolve (CMake + ECM +
Corrosion wrapper, invent hosting, `org.kde.*` id, KDE review).

## Review log

Adversarial review of the first baseline, 2026-10-05 (1 blocker, 12 major, 10 minor). All fixes
accepted; none rejected. Where the reviewer offered alternatives, the choice is named.

| # | Sev. | Issue | Addressed in |
|---|---|---|---|
| 1 | blocker | Backend-side state changes lost between backend and core commit | §0, §4.1 principle 6, §5.5, §5.6, §6.3, §6.6, §6.8-6.10, §6.12-6.13, §7.1-7.2, §10.2, §10.4 (`after_backend_effects_commit`, kill-at-every-point) |
| 2 | major | `IncomingMessage` without author or device | §3.5 `Origin`, §4.2, §4.9, §10.2 |
| 3 | major | Opaque `NetMsgId` with embedded author | §0, §3.1, §3.8, §4.2 `allocate_message_id`, §4.8, §5.4, §7.1-7.2 |
| 4 | major | `IdentityMerged` when both conversations exist | §3.3 `merge_conversation`, §4.9, §8.5 (`UiEvent::ConversationMerged`), S3, S4 |
| 5 | major | No partial transmit outcome; whole group resent | §3.7, §4.2, §4.8, §5.4 `send_state`, §5.5 `send_progress`, §6.7, S5, S11 |
| 6 | major | No relink/unlink semantics | §4.2 `StartLogin.relink`, §4.3 steps 5-6, §6.4, §6.5, S6, S15 |
| 7 | major | Account keys only in `core.db`; overstated shredding | §0, §4.3 steps 7-8, §5.1-5.4 |
| 8 | major | Serial stage 2 with inline network I/O, ~2 fsyncs per message | §1.3, §3.9, §5.6, §6.6 (member under last known state → apply and mark dirty, else park lane), §6.8, §6.12 |
| 9 | major | `Online` at `QueueEmpty` before the inbox drains | §4.2 `catch_up`, §4.5, §6.6, §7.1-7.2, §8.7, S7, S14 |
| 10 | major | WhatsApp events outside the durability hook | §7.1 |
| 11 | major | M1 steps out of dependency order | §6.5 step 8 `initial_sync`, §6.9, §11 (S6, S9, 4409 in S15) |
| 12 | major | Real server first touched in S15 | §6.1, §10.1, §11 S2 (walking skeleton), S8, S11, §12 |
| 13 | major | Out-of-order query results | §8.5 (generations + one query in flight; core-pushed diffs later) |
| 14 | minor | Bounded listener channel drops envelopes | §6.4, §6.6, §10.1 (unbounded channel + delayed acks) |
| 15 | minor | Zone transaction open across awaits | §6.3 (synchronous zone, `now_or_never`) |
| 16 | minor | PNI signature, session usability, durable read syncs | §4.2, §4.8 `pending_commands`, §5.4-5.5, §6.6-6.7 |
| 17 | minor | Account-wide dedup; receive checks; Node check; `marked_unread` | (a) §3.1, §5.4; (b) §3.8, §4.9, §6.13; (c) §10.3; (d) §3.4, §4.2, §5.4, §6.9 |
| 18 | minor | Non-whitelisted 1:1 sends without profile key | §0, §3.8, §6.7, §6.9 (`cannot_send = NotAccepted` chosen) |
| 19 | minor | Contract gaps for WhatsApp history, Threema transactions, broadcasts | §3.4, §3.8, §4.2, §4.10, §7.3, §10.2 |
| 20 | minor | Selectable formatted text impossible with StyledText | §0, §8.3, §8.4, §9.4, S4 (TextEdit + whitelisted RichText chosen) |
| 21 | minor | Image provider on shared read connections; memory budgets ungrounded; undumpable process | §1.3 (S0 baseline 26.9 MB PSS measured), §2.5, §5.4, §8.6 (handle table + third connection for misses), §9.4, S0, S16 |
| 22 | minor | Shared runtime and single writer starve live ingest | §2.5, §4.9, §4.10, §5.4, §7.1 (dedicated WhatsApp DB thread) |
| 23 | minor | Qt's OpenSSL next to static BoringSSL untested | §2.4, §5.3, §10.5, S1 |

Facts updated in the same revision: linker is rustc's rust-lld via `-fuse-ld=lld`, mold dropped
(§0, §2.4, App. A); Node 24.21.0 installed via nvm (§10.3, §12.2); the stray rustup 1.98.1
toolchain was removed (§2.4, §12.2); S0 committed (`51ee9db`) with a measured memory baseline and
a clean `nm -D` (§1.3, §2.4, §11).

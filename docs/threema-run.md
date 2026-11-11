# The Threema run against the real service (RT1)

Outside this tool Kourier has Threema on and links any ID: that was the user's decision of 2026-11-09.
This tool sets `KOURIER_THREEMA_RUN` for the app it starts, and that start keeps every rule of this runbook:
a debug build, a run profile, the list of test IDs, the switch that is off at every end. Never start a run
profile by hand: the app itself shuts Threema out of a start that holds this tool's `threema-run/`
directory (or is driven by `--dev-control`) without the tool, but only this tool's start may reach Threema.

This is the runbook for the first run of Kourier's Threema backend against Threema's servers, with a
test account. Two people work together: the lead types the commands, the user holds the TEST phone and
the PERSONAL phone and types the one value only he may type, the test ID. The tool is
`cargo xtask threema-run` (`xtask/src/threema_run/`). It is the sibling of the smoke tool
(`docs/smoke-run.md`), the WhatsApp tool (`docs/whatsapp-run.md`) and the Telegram tool
(`docs/telegram-run.md`), made for Threema's rules: Kourier links to the TEST phone as one more device
(a QR code, three emoji), and while the phone leads it only follows.

RT1 is ONE run: link, follow, receive, mirror, the phone asleep, the flight mode that makes Kourier the
leader, and a restart. The link is kept at the end, unless the user says otherwise there.

## The rules

- No command here contacts Threema unless it is `start --confirmed-by-user`. The lead passes that flag
  only after the user said go in the same session, for EVERY start. A go in an earlier session counts
  for nothing. Tests, demos, rehearsals and agents use the fake that plays Threema. The rehearsal runs the
  whole tool against it inside `unshare -rn`, with no network at all.
- A real run is never a test and never part of `ci`. `start` refuses inside a test run, outside the
  `kourier.slice` CPU cap (it never runs on the agents server), on a tree that is not clean, on a HEAD
  that is not `main`, on a disk without encryption unless the user accepted that
  (`--allow-unencrypted-disk`), on a clock that is not synchronised (its error bound must be under 16 s),
  and while `Cargo.lock` holds a TLS library older than the pinned bump (the refusal names it).
- THE TEST ID is the user's. He types it, twice, in a terminal of his own, with nothing shown, in
  `threema-run id`. The tool hashes it there and sends only the digest on: the list of test IDs
  (`threema-allowed-ids` in the keeper root) holds the digest of the ID the app may link, and it lives
  from his `id` until the app's first answer on its dev control, seconds in all, and only in a start made
  for a link (`--for-link`). No ID, no digest of one, no phone number, no name and no text is ever on a
  command line, in an environment variable, in a report or in a log: the parser refuses a flag whose
  name could carry one and any value that has the shape of an ID or a digest, without echoing it, and the
  supervisor redacts both shapes in everything it writes. The personal phone's ID is never typed.
- Between runs the profile does not connect to Threema. The tool switches Threema ON in the profile's
  `kourier.toml` for a start and OFF at every end, after the app's exit, a halt or a stop included. A
  start that was killed leaves it on; `state` says so and `stop` puts it right.
- The tool prints and writes counts, words and times only. It never prints a path (the profile's NAME
  only) and never a name, a text, an ID or a link.
- A send from Kourier happens only inside the steps `send`, `group-send` and `send-file`. It is the user's
  own act in the window, or, with `step <name> --lead-sends`, the step's own. Each step opens a window for
  the judge BEFORE anything is sent: at most 4 reflects and 16 boxes of origin `transmit` per step and
  socket. The window closes at the step's end. Any other such frame, in a step or outside one, halts the
  run (`transmit`).
- The lead passes `--lead-sends` only after the user's word in chat in the same session. Such a step makes
  one send: its own marker text into the only chat of its kind (with two it sends nothing), and `send-file`
  with the tool's own small picture as well. The supervisor takes it only from that step's process, only
  while its window is open, and once per step and start. `ctl` still forwards no `send`, no `attach`.
- The user's PERSONAL phone never opens Threema's linked-devices menu. Only the TEST phone does: it is the
  one that lists the device Kourier makes.

## What is needed

1. THE TEST PHONE with its own Threema ID (the TEST ID) and the PERSONAL phone with another, both with
   Threema, in a chat with each other, and a group of both. A group of the run is made within the 7 days
   before it, and nobody changes a group during the run (a group change stops it, see the stop rules).
2. THE BUILD, made with no go: the build contacts nothing. On this machine:

       kcap -t 5400 cargo build -p kourier

   It is the build the tool would make anyway, so the tool's own build in the next step is incremental.
3. THIS MACHINE: a checkout with a clean tree whose HEAD is `main`, the profile on an encrypted disk (the
   profile's database key is a public constant, so on a plain disk the keys lie there readable), a
   synchronised clock, and `kcap`, the CPU cap. Every start goes through it. The keeper root is
   `~/.local/share/kourier-threema-run/`; the default profile is `rt1` (`--profile <name>` for another).
4. TWO TERMINALS: the lead's, and one of the user's own. `id` and `answer` run only in the user's, as the
   built binary, never through `kcap`, a pipe or an agent.

ANOTHER PC (after RT1 on this one). A run there needs the same things, made there: KDE neon on Ubuntu 24.04
with the system Qt 6.11 and Kirigami (the build server's base; never a Qt from `/opt`), the Rust toolchain
of `rust-toolchain.toml`, `kcap` and its `kourier.slice`, and a clean checkout whose HEAD is `main`, from
which `cargo build -p kourier` makes the DEBUG binary (this tool takes no release build) and `cargo xtask
threema-run` is the run tool, exactly as here. The prepared run profile is made THERE, under that PC's keeper
root `~/.local/share/kourier-threema-run/<profile>`, by the tool's own `start --for-link`, as a NEW linked
device that the test phone then lists beside this PC's: a kept profile is never copied from one PC to the
other (it holds this device's keys, and two copies of one device would fight over its slot). The list of
test IDs is written there only by the tool, from the ID the user types in `threema-run id` in a terminal of
his own on that PC, after his go in that session, exactly as here. Refused there as here: a start without
`--confirmed-by-user`, outside `kourier.slice`, on a tree that is not clean or a HEAD that is not `main`, on an
unencrypted disk without his `--allow-unencrypted-disk`, on a clock that is not synchronised, with a TLS
library older than the pinned bump, a release binary, and any value on a command line that has the shape of
an ID or a digest; and the mediator refuses a link when the account already has as many devices as it allows
(close 4111: remove an old Kourier entry on the test phone first).

## Step 0: the rehearsal

`srv 'kcap -t 5400 cargo xtask threema-run rehearse'` from the checkout, on the agents server. It takes
the test lock, builds the app once, stamps it into a throwaway place below `target/thr/`, and runs the
tool's own commands as separate processes, each in `unshare -rn` (no network interface but `lo`),
offscreen and silent, against the fake that plays Threema (`KOURIER_DEV_REHEARSAL=threema`). The user is
played by a pseudo-terminal: `id` is typed into one and every answer too. Its checks, in order:

1. `start` without the user's go on a scratch keeper root is refused and makes nothing;
2. `id` with no run is refused and asks nothing;
3. `start --for-link`: Threema's switch is on, the app's probe registers it, the supervisor waits for the
   ID, no list stands;
4. `id` on the terminal: `taken (8 characters)`, nothing is shown, the list stands 0600 in a 0700 keeper
   root with the digest of the fake's ID and nothing else, and is gone at the app's first answer;
5. `step link`, `sync`, `receive`, `group-receive` and `passive --seconds 5` pass, and `step unlink`
   with a no to its question changes nothing;
6. `state`, then `stop`: the switch is off and a probe no longer registers Threema;
7. `backup` of the stopped profile (with a planted list standing in the keeper root) makes one 0700 copy
   that holds the profile's state and data and neither the list nor the stamped program; the planted list
   is removed by the next plain `start` with its warning, `id` is refused there, `step reconnect` passes;
8. `report` writes its file 0600, `wipe` without its flag deletes nothing and with it deletes the profile;
9. a run with a TEST phone (the scripted fake, never `adb`): `start --for-link --test-phone`, `id --from-test-phone`, `link` with only the emoji's question left to a person, `sync`, `receive`, `group-receive` and `passive`, `unlink` refused without `--user-said-yes`, `state` naming the phone, a relayed answer with no question refused;
10. the leak search over every file the walk wrote (the stamped program aside) and everything it printed
   finds none of the fake's IDs, their digests, its names, its group key or its texts, and no run of 64
   hex digits.

It ends with `REHEARSAL PASS: <N> checks in <S> s` and the list of what it did not show. `--keep` leaves
the place; `--no-build` skips the build (the debug app must be there).

What it cannot show: the real join and its warning page, Threema's own answers, a leader (the fake prints no
count line and no leader line, so `sync`, `passive` and the send steps say "not measured (rehearsal)" for
their frame counts, no real file moves in `files` or `send-file` (it walks `group-send` and `send-file`
with `--lead-sends`, and `send` with a cue for the user's own act), and `phone-sleep`,
`flight-mode` and `kinds` are not walked), `mirror` (the fake's phone has no act that writes an own
message), the removals (`removed-on-phone` and the real end of `unlink`), the refusal of a wrong ID (the
fake judges no ID; the backend's own tests prove that gate), a search of the app's raw stderr, and the
window.

## With a test phone

A run started with `--test-phone <serial>` (the TEST phone's adb serial, for example `host:port`; the
tool keeps it in the profile's state, a start without the flag uses the kept value, the flag wins,
`--no-test-phone` makes the profile forget it) answers the questions about the TEST phone by CHECKS it
makes itself on the phone, and does the TEST phone's acts itself. Without it everything is as described
above and `answer` stays the user's key in his own terminal. `state` shows `test phone: <serial>
(reachable|unreachable)` (an adb connect and get-state, no screen of the phone is opened). The new code
never contacts Threema: `start --confirmed-by-user` stays the only command that does, with the user's go
per start. In a rehearsal the phone is always a scripted fake; no test and no rehearsal runs `adb`.

THE RULES OF THE CHECKS. A check fails closed: a screen that cannot be read, a node that is missing, a
tap that has no effect or a timeout is never a yes. A screen that was read and says no is a no; one that
could not be read pauses the step with "run `step <name>` again" (its progress, a cut included, is kept)
and halts nothing. Every answer is named by its ORIGIN in the step's notes, in the record's
`facts.origins` and in the report: `checked on the TEST phone`, `checked in Kourier's own state`, `typed
by the user`, `relayed by the lead` or `not looked at`; a question with two halves (the warning) names
each half. Every act is told as one fixed sentence (`ACT: ...`). Nothing a phone shows (a name, a number,
a Threema ID, a digest, a message text) is printed, logged, written or put on a command line: texts are
read into memory, compared there and dropped. The texts the step writes or has the personal phone write
are UNIQUE PER STEP ATTEMPT (`kourier-rt1-<slot>-<six random letters>`, chosen at the step's first call
and kept in its carry), so a copy from an older attempt can never count.

THE MARKERS HE TYPES (RT1, 2026-11-05). A marker is counted CASE-INSENSITIVELY with the marker's surrounding
whitespace trimmed (Android capitalises the first letter of a typed text). The `DO-USER` line puts the marker
on a line of its own (`DO-USER: ... The text must be exactly:` then a second line with only the marker), so it
is easy to copy. When the step's own marker is not found but ANOTHER marker of this run (an earlier step's, or
an earlier attempt's) shows more often than its own step recorded it (`facts.obs.markers`: step name, marker,
how often it was counted), the verdict is not `lost`: the note says "not decided: a marker of another step was
sent (a mix-up)" and names that step by its name only, the step ends FAIL with "run `step <name>` again", and
nothing halts; the extra copy then counts as recorded, so only a further one is a mix-up. The same ID-shaped
redaction that masks a value from outside leaves the tool's own upper-case words of eight characters alone
(`PERSONAL`, `QUESTION`, `MENTIONS`, `TERMINAL`: `supervise::OWN_WORDS`). The push mode (`push-own`) is decided
once per step: a resumed call keeps what was recorded with its origin (`facts.obs.push_own`, `push_origin`),
`null` is "not looked at", and the report says `Push mode on the TEST phone (<origin>): ...` or `not looked at`.

WHAT EACH QUESTION BECAME.

| Question | With a test phone |
|---|---|
| `test-phone-in-hand` | CHECK: the SUPERVISOR reads the TEST phone's My ID at the serial it was started with and compares its digest with the one it holds, in its own process; only a yes or no reaches the step; a different ID is a no and nothing is scanned |
| `id` of a link | `threema-run id --from-test-phone` asks the supervisor to read My ID from the phone and keep only the digest; the request carries no digest and a request that does is refused; refused in a run without a test phone; the typed `id` stays |
| `emoji-equal` | stays HIS, typed in his own terminal (`answer`), never relayed and never done by the tool: the dev control does not carry the window's emoji and the phone's trust screen was never surveyed |
| `list-shows-kourier` | CHECK: the linked-devices list holds a row labelled Kourier with this PC's host name and no unlabelled row ("Device" / "Information could not be obtained"); the list must be positively loaded and stable |
| `menu-path-as-runbook` | CHECK: the step walks overflow > Settings > "Threema 2.0 for Desktop (Beta)" by labels; yes when every label was found in order (it proves the path exists, not the one a person used) |
| `phone-warning` | TWO halves: the TEST phone's main screen and notification shade are searched for texts of the app's warnings (checked on the TEST phone); the PERSONAL phone's half is `ASK-USER`; both must be no |
| `pp-read-it` | CHECK in Kourier's state (the own message reads `read`) after `DO-USER`; else `ASK-USER` |
| `kinds-sent` | `DO-USER` for the six, a bounded wait until the three new messages they make are counted and the counts have been quiet, recorded as `not looked at` |
| `phone-locked` | ACT: the TEST phone is put to sleep, CHECK: its wakefulness is `Asleep` |
| `tp-shows-once`, `tp-shows-later`, `flight-off-both` | ACT + CHECK: the phone is woken, the chat is opened (the chat list is scrolled whole; exactly one row of the kind, else the act fails closed), the step's unique text is counted: exactly once is a yes |
| `push-own` | CHECK of the notification setting's switch; unreadable is `not looked at` and never pauses |
| `flight-on`, `flight-off-both` | ACT: Threema is cut off the network on the TEST phone whatever Kourier's role at the start (see below) |
| `pp-delivered` | `ASK-USER`: the line contract names no receipt for that one message, so no counter answers it |
| `fs-line` | `ASK-USER`; no answer in its window is `not looked at` and never stops the run |
| `end-go` | CONSENT: the flag `--user-said-yes` on `step removed-on-phone` or `step unlink` |
| `tp-list-clean` | CHECK: the list holds no Kourier row and no unlabelled row, and was positively loaded (device rows, or the app's empty-list text with no progress indicator, equal over several dumps); else the step pauses |
| the question of a failed link | `state` prints an `ASK-USER [link-failed-after-hello]` line (the removal stays his, the phone's list is not read); `answer --relayed y\|n --question link-failed-after-hello` closes it |
| `mirror`'s act | ACT: the step's unique text is typed into the 1:1 chat on the TEST phone (a draft of an earlier attempt is wiped first) and sent by ONE tap; "send tapped" is written to the step's carry BEFORE the tap, so a later call only counts and never sends again |

The personal phone's texts in `phone-sleep` and `flight-mode` are the step's own unique words, told in
the `DO-USER` line so the step can count them on the TEST phone. Kourier's state carries no chat names,
so the chat rows of the TEST phone are told apart by kind (the group mark); a second row that looks like
the 1:1 chat (or a distribution list, which has no mark) makes the act fail closed.

THE FIREWALL CUT. Real flight mode would also cut the Wi-Fi link the tool needs, so the step "Threema
cut off the network on the TEST phone" replaces it: root `iptables -w` and `ip6tables -w` OUTPUT DROP
rules on Threema's user id, each cut tagged with its own comment (`kourier-cut-<six letters>`), made
together with a timer that runs on the phone and deletes only that cut's tagged rules after an hour even
if this tool dies. Only OUTPUT is dropped: connections stall and the mediator sees a silent loss, though
inbound segments may still arrive until it closes. The step cuts whatever Kourier's role at its start.
Every call checks that the rules still stand and re-arms the timer (a new tagged pair, the old pair out)
when less than half an hour of it is left; a cut that ended early FAILS the step, because the phone was
online while the texts were sent. The step removes the rules itself at the end (also when it fails or
the run halts, but not when a call paused or the phone was unreachable: then the cut stays, on record,
until the next call or the phone's timer) and checks that neither table holds one of its tag.

ASK-USER, DO-USER, CONSENT. Where only the personal phone can show a fact, the step prints one line
`ASK-USER [<question>]: <words>` (the question's name in brackets) and an `ANSWER:` line with the exact
command; the lead asks the user in chat and passes his word with
`target/debug/xtask threema-run answer --relayed y --question <question>` or `... --relayed n --question
<question>` (recorded as `relayed by the lead`; it needs no terminal and is refused in a run without a test
phone, and never for the emoji question). A relay answers only the question it NAMES: when the open question
has another name (or `--question` is missing) nothing is taken and the question stays open. The personal phone's acts stay the user's: the step prints `DO-USER: <words>` and waits for
what arrives. A step that ends the link (`removed-on-phone`, `unlink`) stops before anything is asked of
the judge unless the lead passes `--user-said-yes`, and only after the user said yes in chat.

THE START with a test phone: `kcap -t 900 cargo xtask threema-run start --confirmed-by-user --for-link
--test-phone <serial>`, then, in place of the user's `id`, `target/debug/xtask threema-run id
--from-test-phone` (the lead may run it: nothing is typed or shown).

## The run

All the lead's commands are run from the checkout, one at a time. A step may be called again: a step that
runs out of time ends FAIL with "run `step <name>` again" and goes on from where it was.

1. The lead asks the user for his go. Only then, on this machine:

       kcap -t 900 cargo xtask threema-run start --confirmed-by-user --for-link

   (`--profile <name>` for another profile than `rt1`; `--no-build` reuses the profile's stamped copy,
   which must match HEAD; `--allow-schema-change` starts a build whose database schema differs from the
   profile's, only after a `backup`; `--allow-unencrypted-disk` is the user's word, never the lead's.)
2. The preflight prints what it found (words only) and starts nothing if anything fails. Then the tool
   builds, stamps an execute-only copy of the app into the profile, switches Threema on, runs the app's
   probe (`--print-networks`, which contacts nothing) and starts the app with the dev control. THE ID
   IS WAITED FOR: the app does not start until the user typed it. The wait is 9 minutes.
3. THE USER types the ID in his own terminal, TEST ID, never the personal one:

       target/debug/xtask threema-run id

   It asks twice, shows nothing while he types, takes eight characters (letters in either case, digits),
   and says `taken (8 characters)`. A refused entry says why without echoing it. The command takes no
   value: nothing is pasted after it.
4. The app starts. It has 60 s to answer on its dev control for the first time (the list of test IDs goes
   at that answer); a silent app halts the run (`app-not-up`). DO NOT CLICK IN THE WINDOW IN ITS FIRST
   SECONDS: the window clears its own page stack a moment after the app answers the tool, and a login
   started then is cancelled. Wait until the chat list is on the screen and about ten seconds have passed.
   A cancelled login registers nothing: `step link` says so and is run again.
5. `cargo xtask threema-run step link`. The step asks, in the user's terminal, one key each, through
   `target/debug/xtask threema-run answer`:
   - "Is the phone in your hand the TEST phone?" This comes BEFORE any code is up. A no closes the page and
     nothing is scanned.
   - The user clicks Add Account in the Kourier window, chooses Threema and acknowledges the warning the app
     shows for an experimental network. The link page shows a QR code. Only after his yes AND the code
     does the step say "scan now".
   - On the TEST phone: Threema, the overflow menu (three dots) > Settings > "Threema 2.0 for Desktop (Beta)" > Add device, and scan the code. The scan has 300 s.
   - The three emoji on the window and on the phone: "equal?" A no halts the run (`stopped`): cancel on the
     TEST phone and confirm nothing.
   - After `done`: "does the TEST phone's linked devices list show an entry named Kourier with this PC's
     host name in brackets?", "was the menu path the one the runbook spells?" (overflow menu > Settings > "Threema 2.0 for Desktop (Beta)"), and "does
     either phone warn about an unofficial client, the device or a verification?" A yes to the last halts
     the run (`phone-warning`).
6. The following steps, in this order, each started by the lead on the user's word: `sync` (online, the
   time from the link, the first frame counts), `receive` (one text from the PERSONAL phone into the
   existing chat arrives once), `group-receive` (the same into the group), `mirror` (one text from the
   TEST phone shows once as an own message, `sent`, then `read`), the optional `kinds` (the PERSONAL phone
   sends a quote, a mention, bold text, an edit, a reaction and a delete; Kourier counts, nothing more),
   `passive --seconds 600` (online at every look, nothing sent), `files` (ONE picture, ONE file and ONE
   voice message from the PERSONAL phone raise the 1:1 chat's unread counter by three, and the user says
   they opened and played in the window, and whether the profile picture shows), `send` (one text the
   user sends in the Kourier window shows `sent` in the chat, reaches the PERSONAL phone exactly once and
   shows on the TEST phone as its own), `group-send` (the same into the group), `send-file` (the same with
   one small picture and a caption; with `--lead-sends` each of the three sends by itself),
   `phone-sleep` (the TEST phone locked for
   10 minutes, one text at minute 5), `flight-mode` (the leader test). `steps` lists them with their
   words. A call watches at most 540 s: `phone-sleep` takes two calls, `flight-mode` two or three; each
   call that stops short says "run `step <name>` again" and the next goes on by the clock.
   With `--lead-sends` a send step asks only what the phones show: the TEST phone's half can be checked
   and relayed by the lead, the PERSONAL phone's half is the user's word.
   Only a `--lead-sends` send opens a chat in the app. No other step does: opening one with unread messages would mark it read, and Kourier would
   send that receipt. What a chat shows is read from the list. If the user opens a chat himself, that is
   his act and the step's record says so. The send steps come after `passive`: `sync` and `passive` allow
   no frame of a send on their socket. In a send step a call that stops short keeps the text and the window
   and never asks for a second send; a call after the PERSONAL phone's yes does not wait for the chat's preview again, since the next message to arrive takes it over; a PERSONAL phone that did not show it exactly once halts the run
   (`lost`). A `files` call needs that 1:1 chat closed in the window at its start.
7. `stop`, `backup`, then, on his go, a start with no link and `step reconnect`:

       cargo xtask threema-run stop
       cargo xtask threema-run backup
       kcap -t 900 cargo xtask threema-run start --confirmed-by-user
       cargo xtask threema-run step reconnect

   `reconnect`: online again, no login step, no 4115, the chats of the earlier steps as they were left.
   There is no `id` here and no list: a start that is not for a link needs neither.
8. `cargo xtask threema-run stop` once more, `backup`, `report`. The login is KEPT and Threema is off.

### The two ends, only on the user's word

`removed-on-phone`: the user removes the Kourier entry on the TEST phone while the run lives; the mediator
closes with 4113 and the account reads logged out; nothing halts, the step says `stop` next and then `wipe`.
`unlink`: the user removes the account in Kourier's Settings, Accounts; Kourier drops its own device on the
mediator (acked), the account goes, and the TEST phone's list is clean. Both ask "This ENDS the link: do
you want that now?" first, and a no changes nothing.

### What Kourier writes as the leader

While the phone leads, Kourier writes nothing but its acknowledgements of frames and a job's frames. In
`flight-mode` the TEST phone is off the network, the mediator promotes Kourier, and Kourier writes ONE
unblock, and per incoming message a Reflect and a message ack; per 1:1 text the delivery receipt (a Reflect
and one box); and for a message wrapped in forward security one `Reject` box. The personal phone may show a
line about forward security after the link: it is expected, recorded and never stops the run.

### The step-aside

When Kourier leads and holds a message only the phone resolves (an unknown message type, or the answer to a
group's creator that needs a picture), it asks the mediator for the devices and, if another device is
online, closes its socket with 1000 ONCE and pauses two minutes, so that the phone can lead and take the
message from the chat server. It does so at most once every 15 minutes. That is no halt: the tool reads the
ask, the answer and the closing line as the step-aside's own, does not count its end as a loop of sockets,
and the report names it. A held group change is another thing and stops the run (below).

## The stop rules

A halt names its KIND and nothing else. The run then ends by itself: a link the phone confirmed is let
finish (up to 120 s), a login that waits at its code is closed, the device's drop is waited for when the
link failed after its hello (12 s), then the app is asked to quit and killed after 5 s, and Threema is
switched off in the profile after its exit. Nothing is retried. The lead reads `state` and
`threema-run/report.md`, tells the user the kind, and a new start needs `--after-halt` and his go.

| Kind | What it means | What to do |
|---|---|---|
| `link-refused`, `list-not-taken` | the pairing was refused before Kourier stored or registered anything (the list was empty or did not take, or the ID is not the listed one) | nothing is registered: do NOT scan. A new link is another start with the user's word |
| `link-failed-after-hello` | the login failed, or the app was gone, after the phone confirmed it: a device MAY be registered | on the TEST phone only, open Threema's linked devices and remove any Kourier entry; then `target/debug/xtask threema-run answer`. No new link, no `wipe` before. The report says what the mediator said to Kourier's own drop (removed, not reached, not confirmed): check anyway |
| `needs-action`, `restricted`, `logged-out` | the account says so (a ban, a limit, removed on the phone) | read `state` and the report; the user decides |
| `close-<code>` and `close-error`, `untrusted`, `not-registered`, `first-row-refused`, `clock` | the mediator or the chat server closed the socket with a code the run cannot go on after (4110 to 4115 or a code the spec does not name), a certificate is not one Kourier trusts, the join's first server info was not new, the core refused the first row, or the clock is off by more than 20 minutes | read the report's close table; fix the cause before another start |
| `unasked`, `follower-wrote`, `leader-burst`, `transmit`, `unhandled-<kind>`, `unhandled-other`, `line-unreadable` | the frame counts broke the follower premise or a bound, a payload Kourier does not handle came, or a count line could not be read | the report's socket table has the counts: a finding, stop there |
| `held-<rule>` | a message is held: a group change (`group-sync-needed`, `group-held-before`) arrived, or a rule that is Kourier's own failure or resolves later (`inbound-zone-failed`, `inbound-reflect-unanswered`, `inbound-reflected-zone-failed`, `inbound-queue-full`, `inbound-reflected-queue-full`, `fifo-full`, `directory-down`) | the phone takes a held group change from the chat server once it leads again; make no group change during the run. The two `inbound-reflected-*` words are a store failure or a full job budget of Kourier's own: read the report and the app log, nothing is lost (the message was not acknowledged to the chat server) but the run stops there. The two holds only the phone resolves (`csp-unknown-type`, `creator-answer-needs-picture`) and a second copy of a message already known (`inbound-known`) do not stop it |
| `socket-loop` | more than 3 sockets ended in 10 minutes, or 13 in an hour, that no stop caused | a connection loop: read the report |
| `lost`, `twice` | a text the user sent is missing or shown twice, in Kourier or on a phone | a finding for the step that asked |
| `phone-warning` | a phone shows a warning about an unofficial client, the device or a verification | the user decides whether the run goes on |
| `stopped` | the user said stop (the three emoji differ, or he cancelled) | cancel on the TEST phone and confirm nothing |
| `error`, `exited`, `app-not-up` | an ERROR line of the backend or the core, a panic, the app's exit nobody asked for, or the app's silence for 60 s | read `threema-run/app.log` redacted, and the report |
| `count-lines-missing` | the account has been online for 150 s with no count line from the backend: the judge reads nothing (the log filter or the log path drops the lines, or the backend stopped printing them), and a run nobody judges must not go on | read `threema-run/app.log` and check that the app's log filter still prints the backend's INFO lines; fix the cause before another start. A real run only: the rehearsal's fake prints no count line |
| `list-not-removed` | the list of test IDs could not be removed from the keeper root when the app answered (or at the run's end) | `threema-run state` shows whether it stands; `threema-run stop` removes it, else remove the file named `threema-allowed-ids` (and `threema-allowed-ids.tmp`) in the keeper root by hand before any other start |
| `app-readable` | the app's environment or memory map could be read by the user's other processes right after its start, although its copy is execute-only | the run ended; tell the user, the cause is the machine's `fs.suid_dumpable` or a changed copy mode: a new start only after it is found |

A rule that does not stop: the first D1 line (Kourier took the lead), the drop's own lines, an E4 refusal of
a server message that does not close the socket, the step-aside, and a hold of the three tolerated kinds.

## What to send back

The lines each command printed, and `threema-run/report.md`: the profile's name, the build commit, the
steps and what each saw, the sockets' frame counts, the lines of the contract and their tally, the leader
observations, the push mode, the step-aside when one happened, and the table of the questions the run
settled or found open. Counts, words and times, never a value. Never the profile directory, never
`app.log` unread, never a screenshot with names.

## Stopping, keeping, backing up, undoing

- `cargo xtask threema-run stop` asks the app to quit and waits for it; the login stays, Threema is
  switched off in the profile. A supervisor that does not end in time leaves the switch on and the words
  offer `--force`.
- `cargo xtask threema-run backup [--to <dir>]` copies the kept profile while no app runs on it (the
  profile's lock is taken; it refuses while a supervisor or the app holds one) to
  `backups/<name>-<UTC time>` in the keeper root (0700), or into `<dir>`. It never goes into the repository,
  its worktrees, the build directory, the user's projects tree (it syncs to the server every hour), the app's own data
  directory, another tool's profiles, a link or the profile itself, and it refuses a disk without
  encryption. It never copies the list of test IDs, the stamped copy of the app or the app's plaintext
  playback copies. The copy holds the account's keys (the identity's client key and the device group key)
  and is as sensitive as the profile itself. Restore only a copy taken after the last run, as a whole
  directory with the profile's own name. The profile name `backups` is reserved.
- `cargo xtask threema-run wipe --yes-delete-the-login [--profile <name>]` deletes the profile directory for
  good (the app's data, the stamped copy, the logs) and a leftover list of test IDs. Without the flag it
  deletes nothing and says so. The TEST phone keeps listing the device until the user removes it there, so
  do that first: a wiped profile can no longer drop its own device. It refuses while a question of a failed
  link is open and while the app runs (the supervisor's lock or the app's own is held), and it never touches the backups or another profile.
- A kill of the tool leaves Threema switched on in the profile: `state` says so and `stop` writes the switch
  off. A kill of the supervisor between `id` and the app's first answer can also leave the list of test IDs
  (or the temporary file it is written through) in the keeper root: `state` shows it and `stop` removes it;
  the next start and `wipe` remove it too. A `start` that is ended by a signal (a Ctrl-C, the shell call
  ending) while it waits puts the switch back off itself when no supervisor was started yet.

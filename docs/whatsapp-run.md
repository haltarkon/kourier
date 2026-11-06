# The WhatsApp run against the real service

This is the runbook for the first runs of Kourier's WhatsApp backend against WhatsApp's real
servers, and for every later run on the same login. Two people work together: the lead types the
commands, the user holds the phones and types the test number himself. The tool
is `cargo xtask wa-run` (`xtask/src/wa_run.rs`). It is the sibling of the smoke tool
(`docs/smoke-run.md`), made for WhatsApp's rules.

WhatsApp can ban or restrict an account that uses an unofficial client. Nothing here removes that
risk. The run therefore uses a dedicated TEST number, never the personal account, goes slowly,
sends only what R2's small fixed list names (and only after his go), and stops at the first sign of a restriction.

## The rules

- No command here contacts WhatsApp unless it is `start` with `--confirmed-by-user`. The lead
  passes that flag only after the user said go in the same session. A go in an earlier session
  counts for nothing.
- A real run is never a test and never part of `ci`. `start` refuses inside a test run (the test
  lock, nextest) and outside the `kourier.slice` CPU cap.
- The QR code is scanned by the TEST phone. Nothing is ever linked to the personal account. The
  personal phone only sends test texts to the test number. The first link is by QR code only.
- The test number is typed by the user in `wa-run tail`, in a terminal of his own, with nothing
  shown while he types: the WHOLE number with its country code, 8 to 15 digits, so that a wrong
  operator code is refused too. A plus, spaces and dashes may be typed and are dropped; any other
  key refuses the entry at once, loudly ("only digits, +, spaces and dashes"), so a typo is never
  dropped in silence. The prompt asks twice and the two entries must match. The lead never sees the number, never types it and never puts it on a command line,
  in a file or in a report. It reaches the app only when the tool starts it.
- Kourier checks the number before the phone gets a device: a phone whose number does not end in
  the typed digits (all of them, when he typed the whole number) is refused, Kourier never signs
  that pairing and keeps nothing of that account.
  This does not say that the other phone shows nothing: a refused scan may leave a notice there.
- The login is kept. Nothing in a default run ends it. `stop` only stops the app.
- No step of R1 sends. A run started without `--for-send` forwards no send, no reaction, no edit, no delete and no typed
  login on its dev channel, whatever a step asks; the send steps below exist only for the send run (R2). Kourier
  starts no chat.
- What a START asks (W6a2, read-only, no step of the tool): after the connection is online, never before, the app
  lists the groups it is in and asks their data in ONE batch (two requests, once per start, under one bucket of
  at most one group fetch per 30 seconds and ten per ten minutes; a refused answer is not retried in that start),
  and the phone's blocklist at most once a day (the first start after a link asks too). Nothing is sent that
  changes anything: the only frames the app can write for a user's command are a group admin's delete of another
  member's message and the 1:1 disappearing timer, and no step of this tool issues either.
- A step waits at most 9 minutes per call and says when to run it again. A long step goes on from
  the record it left.
- One fresh link a day. A second link within a day needs `--allow-second-link-today`, which is the
  user's call.
- On a restriction, a ban, a "needs action" state, a logout from the phone or a usage-statistics
  frame, on digits that did not reach the app, and on a restriction shown in the window the run
  halts. Nothing is retried. Only the kind is reported. A new `start` needs `--after-halt` and the
  user's word. A halt that is a risk to the account (a usage-statistics frame, digits that did not
  arrive, a ban or a hold) stops the app and keeps the login: a login that still waits at its
  warning or its code is ended first by the supervisor itself (the page change to the list; the
  step says "closed" only when the app answered ok, otherwise it says to run `wa-run stop` now),
  a link the phone has answered is let to finish, and then the app is asked to quit and killed
  after five seconds. The login that is kept stays.
- Nothing the tool prints or writes names a person, a number, a JID or a message text. It is safe
  to paste into a chat or a report. The profile directory itself is not safe to share.
- No agent reads, writes or deletes anything in the kept profile. Only `cargo xtask wa-run` does.

## What is needed

- A dedicated TEST SIM in a real Android phone with WhatsApp installed and used normally for some
  days before the run (a few chats, a group, some messages). A brand new number that links a
  client at once is the pattern WhatsApp restricts.
- The personal phone as the peer, with a 1:1 chat with the test number and a group that holds both
  numbers, both started on the phones. Kourier starts no chat.
- The whole test number (see the rules). Only the user knows it.
- This machine: a checkout of the repository, a clean tree on `main`, the encrypted disk under the
  home directory, and `kcap`, the CPU cap. Every build and start goes through it.
- The user's terminal for `tail`, outside the assistant's own terminal panel.

## What linking changes on the test account

- One linked device appears in the phone's Linked devices list, with the name the backend chose.
  WhatsApp may show a notice about it.
- The phone then pushes its message history once per link, in batches, over several minutes. If
  Kourier is stopped hard during the push, part of that history may never arrive: the phone sends
  it once.
- A linked device that nobody uses for 30 days is logged out by WhatsApp, and the phone itself
  must be online at least every 14 days or the linked devices are dropped. The tool warns from 21
  days without an online start. Start the app and let it come online to keep the login alive.

## Step 0: the rehearsal

`kcap -m 8G -t 1800 cargo xtask wa-run rehearse` runs the whole tool against a fake that plays
WhatsApp: a throwaway place below `target/`, no network at all (a user namespace without any
interface), offscreen and silent. It builds the app first, then walks about four minutes of
commands: `start` without the go is refused; a scan by the wrong phone is refused before anything
is signed; the right phone links; the digits stand in the app's environment and nowhere else; the
history push is stopped hard once and finished after a restart; `passive`, `backup`, a restart
that reconnects without a code; `add_account` and `quit` are refused, and so is `send` in a run
started without `--for-send`; `receive`, `group-receive` and `read` run with the fake phone writing
on cue; then the read-only steps of R1 (`settings`, `groups`, `call`, the three archive cases and
the peer's react, edit and delete), with the fake phone archiving, pinning, muting, calling, reacting,
editing and deleting on cue and the user's y or n typed in `wa-run answer` the way he types it, the fake's
frame count staying at 0; then, in starts with `--for-send`, the resend (killed in the air and finished
after a restart), the five send steps (the fake acks each frame and counts it: 9 of the 10 a start
allows) and the refusal of the eleventh; a start with `--call-builtin` that the app's own warning
line confirms and the plain starts around it that show none; `wipe` without its flag deletes nothing;
`report`. Since the test phone came, the read-only steps and the peer's acts run against a SIMULATED TEST phone
(`start --test-phone fake:rehearsal`, accepted in a rehearsal's place only): the steps check its screens and do its acts,
the acts are mirrored into the fake that plays WhatsApp, the lead's relayed word (`answer --relayed`) and the resend's
consent flag (`--user-said-yes`) are walked, and one run of `step groups` with the serial taken away keeps the typed path
proven. The rehearsal's user types the fake's WHOLE number with a plus, as the
user will. Last it greps every file it wrote (and the backup and everything the commands printed)
for what the fake is made of and ends with `REHEARSAL PASS` and the app's memory. It searches for
exactly this: the three synthetic names (the other person, the group, the account), the two
synthetic numbers, the wrong whole number and both tails, the three core ids (the account's and the
other person's LID and the group's number), the four stems of the pushed history texts and the stem
of the arrival's, the two prefixes of the message ids, the device name, the tool's own fixed texts
of the send steps (the resend's and the edit's among them), and a JID (digits before
`@` and a WhatsApp server). The same strings stand in the fake as constants and a test pins the
copies. The look at the process tree checks that the typed digits, with or without the plus, stand
in the app's environment and nowhere else. It cannot show the warning page's tick, the real
pairing, WhatsApp's history size or read receipts.

On the agents server the rehearsal is `srv 'kcap -t 5400 cargo xtask wa-run rehearse'` from the
checkout: there the build container's own limits are the cap. A real `start` runs only on this
machine, inside `kourier.slice`.

## Step 1: start

1. The lead asks the user for his go. Only then:
   `kcap -t 900 cargo xtask wa-run start --confirmed-by-user --for-link --profile <name>`
2. The preflight checks the tree, the schema, the disk, the cap and the profile, builds the app,
   makes a stamped execute-only copy of it and starts the supervisor. It prints one line to paste.
3. The USER pastes that line into his own terminal: `target/debug/xtask wa-run tail --profile <name>`
   and types the whole number twice. Nothing is shown. It prints only `taken (N digits)`.
4. The app starts after the digits. `wa-run state` shows the app, its memory and the tail as
   `waiting`, `typed` or `sentinel`, never the digits.

## Step 2: the steps, in order

`cargo xtask wa-run steps` lists them. Each is run as `wa-run step <name>` and writes a record to
`steps.jsonl`; `wa-run report` turns the records into `wa-run/report.md`.

1. `step link`: in the Kourier window the user reads the warning, ticks the box and continues. The
   tool asks for the account, says "scan now" only while the number check is on, and waits for the
   QR code. On the TEST phone: Linked devices, Link a device, scan. PASS when the login is done and
   the account is the only one. A phone with another number fails the step with its own sentence,
   leaves no account directory, and a second `step link` in that start is refused.
2. `step history`: waits while the phone pushes its history (the account banner says so), then until
   the number of conversations stays the same for a minute. A call that runs out of time says so
   and the next call goes on. The notes carry counts, never titles.
3. `step history-interrupt`: ONCE, while the push is shown and one more conversation has arrived,
   the app is killed with SIGKILL. Then `cargo xtask wa-run stop --profile <name>`, then
   `kcap -t 900 cargo xtask wa-run start --confirmed-by-user --profile <name>` (no `--for-link`,
   no digits are needed) and `step history` again: its notes say whether the phone sent a chunk
   again. Skip it on the user's word. THE KILL ALSO LEAVES ALL FIVE APP-STATE COLLECTIONS OWED (the link session
   moved every one and no quiet quit cleared them), and the start-up resync asks a collection at most once per
   6 h, counted from the link: the restarts of this step and of the steps after it, all inside those 6 h, ask
   nothing. The first start 6 h or more after the link (R2's, as planned) re-reads all five once, one after
   another, right after `Online`: that is the designed repair of a kill, not a fault, and the run report of that
   start carries the five counts.
4. `step passive --seconds 300`: sends nothing, online at every look, no error, and the usage
   statistics counter looked at frames and sent none.
5. `cargo xtask wa-run stop --profile <name>`, `cargo xtask wa-run backup --profile <name>`,
   `kcap -t 900 cargo xtask wa-run start --confirmed-by-user --profile <name>` (no `--for-link`),
   then `step reconnect`: online again with no QR code and no login step.
6. `step receive`: on the PERSONAL phone send ONE short text to the test number in the existing 1:1
   chat. PASS when exactly one new message from others arrived once in an existing direct chat.
   A message that opens a chat Kourier did not have fails the step with its own sentence.
7. `step group-receive`: the same, in the group both phones are in.
8. `step read`: opens the 1:1 chat, and the window needs the focus (click its title bar) for the
   chat to be marked read. PASS when Kourier counts no unread message in it. The read receipt to
   the peer and the sync to the test phone come with a later step; this one checks Kourier's own
   count only.

## The read-only steps of R1 (T2)

These steps send nothing (the tool's dev channel forwards no send and no act, and in a rehearsal the fake's frame
count stays where it was). Each says what the user does on a phone; the ones that ask a question are answered by the
user in a terminal of his own with `target/debug/xtask wa-run answer --profile <name>`: one key, `y` or `n`, read from
the terminal and from nowhere else.

9. `step settings`: on the TEST phone he archives ONE chat, pins ANOTHER and mutes the pinned one. PASS when, within two
   minutes, the core's own row says archived (the shown list hides an archived chat, so it is not read from there) and
   the shown list and the core's row agree on the pin and the mute.
10. `step groups`: the group `group-receive` found shows its members and at least one admin in the core's row; the
    counts are printed, and he says with `wa-run answer` whether the TEST phone's group info shows the same.
11. `step call`: he calls the test number from the personal phone and answers on the TEST phone. PASS when the call
    line of the 1:1 chat says answered (on another device: Kourier never rings or answers) and he has said with
    `wa-run answer` whether the TEST phone rang.

The next three are the archive cases of W6a1's re-check: they record what the phone does and what Kourier does, and they
must agree (Kourier follows the phone). All three need the TEST phone's "Keep chats archived" OFF (Settings > Chats), the
1:1 chat `receive` found, and read the archive from the core's row. The personal phone writes or calls; the user
archives on the TEST phone; the tool sends nothing.

12. `step archive-message`: he archives the 1:1 chat on the TEST phone, then the personal phone writes ONE text into it.
    The step says whether the TEST phone brought the chat back (he answers `y` or `n`) and whether Kourier did, and
    passes when the two agree.
13. `step call-then-archive`: the personal phone calls into the 1:1 chat (nobody answers), THEN he archives the chat on
    the TEST phone. PASS when Kourier shows it archived (a call line is newer than the archive's range: W6a1 NEW-1). If
    Kourier does not, he is asked whether the TEST phone shows it archived: `y` is the finding, `n` means the archive
    was not made.
14. `step call-into-archived`: the 1:1 chat is archived on the TEST phone (the step asks him to archive it if it is not),
    the personal phone calls into it. The step says whether the TEST phone brought the chat back (his `y` or `n`) and
    whether Kourier did, and passes when the two agree.

`wa-run report` also prints, for every start of the app in `wa-run/app.log`, the counts of the start-up resync of rule
(b) (collections asked, synced, unsynced, fatal, spaced, held back, reconciled) and of the group fetch (asked,
answered, how many answers the server flagged `missing_participant_identification`, how many were truncated, and
so how many groups state no member list from that fetch: a flagged answer leaves admin deletes in the group parked).
Counts only, never an id.

## With a test phone (the automated path)

`start --confirmed-by-user --test-phone <serial>` (for example `--test-phone 192.0.2.19:5555`, an adb serial and never
the number) turns the TEST phone into something the steps use themselves. The serial is kept beside the run state
(a later start without the flag keeps it, a flag wins), `wa-run state` says `test phone: <serial> (reachable)` or
`(unreachable)`, and a start without a test phone works exactly as described above: every question is then put to the user
in `wa-run answer` in his own terminal, and nothing here changes it.

What changes with a test phone:

- Every question about the TEST phone is answered by a CHECK the step makes on its screen (over adb, with `uiautomator`
  dumps; no screenshot is taken). The check FAILS CLOSED: a screen that cannot be read, a node that is missing, a tap
  that took no effect or a timeout is a "no" or the step's own FAIL ("run `step <name>` again"), never a pass. A chat on
  the phone is picked by kind and rank (the n-th group or 1:1 chat of the list, ordered the way Kourier orders its list)
  and the pick is checked on the phone (the row is taken from a fresh dump right before the press, and the selected row
  must be the one asked for, else the selection is backed out and the step fails; two rows with one name and kind fail
  closed) and by its effect in Kourier; a pick that is not the chat Kourier knows can read as "the archive did not
  reach Kourier", which the step words as "or the wrong chat" (check by hand). The step's record and `wa-run report` name where each answer came
  from: `checked on the TEST phone`, `typed by the user`, `relayed by the lead` or `not looked at`.
- The TEST phone's acts are done by the step: it archives, unarchives, pins and mutes through the selection toolbar
  (a long press of about two seconds; no chat row is ever tapped, the delete button and the "Keep chats archived" switch
  are never touched), answers and ends a call, and checks each act afterwards on the phone's own lists. It puts the chats
  back before it ends, and an act that fails after it may have taken effect is undone and noted in fixed words; if an undo
  cannot be confirmed the note says to put that chat back by hand. WhatsApp's registration block screen stops every act with one sentence
  (`the TEST phone shows WhatsApp's registration block screen: stop and tell the user`). No message is sent from the phone; what does go out is the phone app's own traffic for an archive, a pin or a mute, and
  the audio of an answered call. The step answers a call ONLY when the caller on the call screen is the 1:1 chat of the run
  (the name compared by hash in memory); any other caller is never answered (`a call from someone else`). If the end of an
  answered call cannot be confirmed the step prints `ASK-USER: Hang up the call on the TEST phone now`, fails, and `state`
  warns that a call may still be open.
- Nothing a name, a number or a text on the phone says is printed, logged, written or put on a command line: a chat is
  known by a hash made in memory, counts are parsed in memory, the notes say fixed sentences.
- Which questions the lead may relay is the SUPERVISOR's fixed table (`resend-once`, `resend-shows`), taken only in a run
  the supervisor knows has a test phone (the profile's serial file at its start); a client's word never makes a question
  relayable. Consent is a flag. `step resend` with a test phone stops before the act ("needs the user's consent") unless the lead
  passes `--user-said-yes`, and he does that only after the user said yes in chat to the question the message names.
- A look that only the PERSONAL phone can make is ONE line `ASK-USER: <words>`; the lead asks the user in chat and
  relays the word with `wa-run answer --relayed y` or `--relayed n` (no terminal; taken only for those two questions in a
  run with a test phone, recorded as `relayed by the lead`). The user's own terminal answer stays possible.
- An act of the PERSONAL phone stays his: one line `DO-USER: <words>`, then the step waits for the arrival in Kourier.

What each question became:

| Step | Before | With a test phone |
|---|---|---|
| `step groups` | he opens the group's info and answers y or n | the step opens it, counts members and admins in memory, equal to the printed counts is the yes |
| `step call` | he answers the call and says whether the phone rang | `DO-USER: Call the test number from your personal phone now`; the step sees the ring on the TEST phone, answers there, holds about 15 s, ends the call there, and the Calls tab's newest row must be an incoming call, not missed |
| `step settings` | he archives, pins and mutes | the step does the three acts and undoes them |
| `step archive-message` | he archives, and says whether the phone brought the chat back | "Keep chats archived" is read (ON stops the step; the tool never flips a setting), the step archives, `DO-USER` for the text from the personal phone, the answer is read from the phone's lists |
| `step call-then-archive`, `step call-into-archived` | he archives and answers | `DO-USER` for the call; the step archives, reads the phone's lists, and puts the chat back |
| `step resend` | y or n to `resend-ok` in his terminal | `--user-said-yes` on the step |
| `step resend` (call 2) | y or n to `resend-once` or `resend-shows` | `ASK-USER` and `answer --relayed y|n` |
| `step peer-react`, `step peer-edit`, `step peer-delete` | `PHONE:` line | `DO-USER` line, same waits |

UNPROVEN on the real phone until its first run: the incoming-call screen (found by a WhatsApp call activity or an
Answer control anywhere on the screen, answered by that control or a swipe up, ended by an End call control), the
"Archived" entry of the chat list, the pin and mute states read from the selection toolbar's button descriptions, and the
rank pick of a chat. The rehearsal walks all of it against a simulated TEST phone (`--test-phone fake:rehearsal`, in a
rehearsal's place only), which proves the tool and not the screens.

## The send steps of R2 (T2, T-3a)

These steps send, so they run only in a run started with `start --confirmed-by-user --for-send` after the user's go, on a
profile that has a login (never with `--for-link`). What the supervisor holds to, whatever a step asks: the texts are the
tool's own fixed constants, named by an id (nobody gives it a text); a send goes only into the two chats `receive` and
`group-receive` remembered, only while that chat is the open one, and only after the supervisor itself has SEEN a message
of the peer in that chat that arrived in this start (the step takes a mark, asks him for one text from the personal phone
and asks the supervisor; a step cannot claim it); a react, an edit or a delete takes only a message the tool sent in this
start. The chat and the message are not only looked at a moment before: every send names its chat, and every act its chat
AND its very message, in the command itself, and the app checks both on its own thread in the same call that sends or acts.
A click into another chat between the look and the frame, or a message of the phone's that arrives meanwhile (the app's
old "newest own message" rule would take it), is refused by the app with nothing sent or done; a frame that would leave
without that guard is refused by the tool before it reaches the app. The app's refusal is a nack like any other and halts
the run. EVERY frame counts toward the budget (a send, a react and its removal, an edit, a delete): 10 in one start of the
app, 30 in one profile, counted before the frame leaves; `start --for-send --reset-frame-budget` starts the profile's
count again and is the user's word, never an agent's. DURING R2 THE WINDOW IS NOT USED: its composer and its message
menus stay untouched (what he sends or reacts to there is outside the budget, and the supervisor halts it as an unforwarded
frame). His y or n (`wa-run answer`) and his digits (`wa-run tail`) are taken by the supervisor only from a process in the
foreground of a terminal (a raw write to the socket is refused and the question stays open), a send start remembers no chat
(`receive` and `group-receive` are R1's), and the resend record is closed only after his answer to `resend-once` (or `resend-shows`, below); the last
guard is procedural, because a program can drive a terminal of its own: only he runs `wa-run answer`. THE TOOL COUNTS THE WIRE, not only what it forwarded: the backend
says each frame of Kourier's send path in two fixed log lines at its one write point (`a send frame was written` and
`a send frame was answered`, a kind and a number and nothing else), and the supervisor counts them. ANY nack halts the
run and STOPS the app: the app refusing a frame, an action that failed, a message of this run shown as failed, a send
whose message does not show, an `answered` line with any outcome but `acked` (a 429, a 5xx, a refusal), or no
`acked` line within 15 seconds of a frame the tool forwarded (well under the 30 seconds after which the core sends a
frame that got a 5xx again by itself, and the 75 seconds of the backend's own ack wait). The 15 seconds run from the TOOL'S
count of the frame (for a frame the last start left owed, from the moment the account is online), not from the write, so the
halt word says how far the frame got: `no-ack` when the backend's `written` line came and no `acked` line followed, and
`not-written` when no `written` line came either. The app's own wait before the wire (the first reach-out pull's hold, a
silent reconnect's catch-up, a gate) therefore halts the same way as `not-written`. What the next start owes depends on
why the frame did not reach the wire. After a WAIT before the wire (a hold, a catch-up, a gate) the app sends the frame in
the next start, and that start owes it. After a FINAL failure before the wire the frame never comes, and the step's dev
row then shows the message failed for good; the next send start halts `not-written` again when its owed frame does not
come. The exit is the user's word, never the agent's: show the lead the halts and the message's row, and only on his word
start with `--for-send --forget-owed-frames` (see "THE USER'S WORD" below). A `written` line beyond the frames
the tool forwarded in this start and the start owes (see below) is a frame the tool did not forward, most likely the
core's own retry or a send made in the window, and halts the run as an unforwarded frame. The halt records a code word,
nothing is retried by the tool, and a new start needs `--after-halt` and his word. A halt cannot cancel the core's queued
retry of a frame: it only stops the app before the retry runs. The retry is then OWED to the next start: the state file
keeps the number of frames forwarded (or owed) whose `acked` line was not seen (`unacked`, written before the frame
leaves, lowered by each `acked` line or settled as below), and the next start owes that many frames. They are counted into its ten and the
profile's thirty before the app starts (a start without room refuses), the start must be `--for-send` (a plain or a
`--call-builtin` start would send them uncounted), and `wa-run state` and `wa-run report` say "owed from the last start:
N". Their `acked` lines settle them; any other outcome halts as above. `--reset-frame-budget` starts the profile's count again but KEEPS the owed frames: they stay owed and are counted into the fresh count (the app sends them again whatever the count says).

WHICH ANSWERS ARE FINAL AND WHICH ARE WAITED OUT. The backend's `answered` line also says `again=`, whether the app sends that
frame again by itself. `again=no` is a FINAL answer (a refusal 463 or 475, a 404, a 400 or 479, any other or unreadable code):
the frame is SETTLED, not owed, so no later start waits for a frame that will never come; the run still halts, and the halt's
code says which answer it was. `again=<seconds>` is an answer the app waits out (a 429 five minutes, a 421, 500 or 503 thirty
seconds): the frame stays owed, the state file records the earliest time the app sends it again (`owed_not_before`), and a send
start before that time is REFUSED before it begins; the refusal names the local wall time, and he starts after it. An answer
with no `again=`, or one that cannot be read, counts as `again=0`: the frame stays owed, as a missing ack does. A frame the
app's own guard refused (the code `guardRefused`: another chat was open, the message was not one of the open chat's, or the
composer was not plain) never reached the core, so it is settled too; every other refusal, a timeout or a lost answer of the app
leaves the frame owed. A frame nobody answers at all (the 15 seconds above) is not a final answer either: it halts as `no-ack` or
`not-written` and stays owed, like `again=0`, until it is acked or the user's word forgets it (next paragraph). A settled frame
stays counted in the budget: no frame is ever given back.

THE USER'S WORD THAT AN OWED FRAME WILL NOT COME. `start --confirmed-by-user --for-send --forget-owed-frames` is a flag like
`--reset-frame-budget`: only with `--for-send` (without it the start is refused, "Nothing was started"), and only the user's
word, given after he and the lead looked at the halts and at the message's row. It makes the owed count 0 and takes
`owed_not_before` away; the old values stay in the state file as `owed_forgotten` (when, how many, until when), and `wa-run
state` and `wa-run report` print them. `frames_used` does not change: no frame is ever given back. If the core does send a
forgotten frame later, the count of the wire counts it and halts the run as an unforwarded frame, which is the right end. An
open resend record is marked `forgotten` and stays open: only `step resend`'s call 2 closes it, after his answer. Three doors
leave a frame owed that will never come: a final failure before the wire (a `not-written` halt, above); a retryable answer
(a 429 or a 5xx) whose wait passes the message's 24 hour budget, which the core turns into a message failed for good while
the answered line said seconds; and the core's at-once retry after a `connection-ended` answer, which the count of the wire
counts as an unforwarded frame and which leaves one frame owed twice. The flag is the exit from all three.

15. `step send`: after his text from the personal phone, one fixed text into the 1:1 chat. 1 frame.
16. `step group-send`: the same into the group. 1 frame.
17. `step react`: the tool's own text, a thumbs-up on it, the thumbs-up taken back. 3 frames.
18. `step edit`: the tool's own text and its edit. 2 frames.
19. `step delete`: the tool's own text and its delete for everyone. 2 frames.

Each step ends by checking that the supervisor let through exactly the frames it budgeted, that the backend's own count
lines say the same (that many frames written and that many acked) and, in a rehearsal, that the fake took exactly that
many. The five steps together are 9 frames of the 10 a start allows.

## The peer's acts and the one resend (T2, T-3b)

The next three steps send no frame of the budget: the user acts on the PERSONAL phone and the tool only watches what
arrives, so each runs under a count of 0 frames (the supervisor's and, in a rehearsal, the fake's); opening the chat may
send the read receipt a window sends when a chat is read. They need no send run, but run in one. They work in the 1:1 chat
`receive` found.

20. `step peer-react`: he reacts to one of the newest messages of the chat. PASS when a reaction of someone else shows that
    the baseline did not hold.
21. `step peer-edit`: he sends ONE text and the step takes it, then he edits THAT text. PASS when its row is marked edited.
22. `step peer-delete`: he sends ONE text and the step takes it, then he deletes THAT text for everyone. PASS when its row
    is a deleted notice. A call that runs out of time after it saw the text goes on from that text.

The last send step is the same-id resend after a kill (his answer in W5: Kourier stops the second a message leaves and
cannot know whether it arrived, so at the next start the core sends it again with the SAME id, and WhatsApp very likely
drops the repeat). It runs only with his OK in that session and takes two frames of the budget, so it needs room for
both in the start it runs in (the five send steps use 9 of the 10; run it in a start of its own, before them):

23. `step resend`, in two calls, one in each start. Call 1, in a send run after the peer wrote: the step asks him, and he
    answers y or n with `wa-run answer` in his own terminal; the SUPERVISOR checks his y itself, takes the frame the resend
    may owe, and only then lets the one fixed text go; the app is killed with SIGKILL at once. Then `wa-run stop`,
    `kcap -t 900 cargo xtask wa-run start --confirmed-by-user --for-send --profile <name>` and `step resend` again, FIRST in
    that start: when the ack of the first frame was not seen before the kill, the start owes that frame, has counted it in
    its ten and holds the message as the tool's own (a failed row of it halts the run), the app sends the message again by
    itself, and the step passes when the SAME message reaches the server,
    no second message of ours shows, and he says with `wa-run answer` that the PERSONAL phone shows it ONCE. Twice is a
    finding (the assumption is wrong). What happened in the instant of the kill (the frame never left, left without an
    ack, or was acked) is not seen from the tool, only whether the backend's `acked` line came before the kill: if it did,
    nothing is owed; all three are inside the two frames counted. Without his y nothing is
    armed, sent or killed. In call 2 the backend's lines must agree with what the start was owed (the owed frame written
    once and acked once). In a REHEARSAL the fake holds back the ack of that text the first time it takes it, so the kill
    really comes before the ack and the resend is real: the fake must take exactly one frame in the second start, and it
    must be a repeat of an id it already holds (a fixed line of the fake, counted by the supervisor), which it drops and
    answers.

    His y is for THAT start. The supervisor writes the record under the frame gate: `armed` (his y, the reserved frame, the
    number of the start), `sending` BEFORE the resend text leaves, `sent` with the message once its row shows. An `armed`
    record of an earlier start (the step failed before the text left) is dropped when the next start begins, and the step
    asks him again; the frame it reserved stays counted. When the text left but the step never saw its message (the app's
    answer was lost, no row showed, the run halted), the record is `sending` or, after a halt, `unknown`: the next start
    owes the frame like the other, but there is no message to compare, so call 2 says so, sends nothing, waits for the
    backend's own lines (the owed frame written again and acked) and closes the record on his look at the personal phone
    (once, or twice as a finding). While any such record is open no second resend can be armed, and a start without
    `--for-send` (a plain one and `--call-builtin`) is refused ("run `start --for-send` first, and `step resend` for its
    call 2"). When the text's frame was settled WITHOUT an ack (a final answer such as a 463, the app's own guard refusing
    it, or a frame that never left), the record says why (`settled_by`), and call 2 says so instead of reading the empty
    owed count as "the ack was seen": it sends nothing, and asks only whether the text shows on the PERSONAL phone at all
    (y: it shows, n: it does not). The record closes on his answer, either way, and the step fails: no repeat was ever
    sent, so neither answer says anything about the same-id assumption. When the owed frame was forgotten on his word
    (`--forget-owed-frames`), call 2 says so, notes that the backend's lines say nothing of the frame, and asks the normal
    once or twice question.

## The call under upstream's handler (T2, T-4, his D9)

D9 is the user's open question: upstream's call handler sends an offer-ack for every incoming call, and Kourier's rule is
to send nothing for a call. R1's `step call` shows what the TEST phone does with Kourier's handler. R2 shows it with
upstream's, in ONE start of its own:

1. The user says go. `kcap -t 900 cargo xtask wa-run start --confirmed-by-user --call-builtin --profile <name>`. The start
   prints a warning line; it is refused with `--for-link` and with `--for-send` (this start sends nothing), on a profile
   with no login, and from a copy of the app that is not a debug build (the backend itself refuses the switch in a
   release build). The tool sets exactly `KOURIER_WA_CALL_BUILTIN=1` for the app of THIS start and nothing it inherited:
   every `KOURIER_*` variable of the shell is taken out, and the launch order that carries the flag is read once and
   removed. The next start, without the flag, has no such variable.
2. `step receive` (the 1:1 chat is remembered), then `step call`, the same call as before. The step proves which handler
   the start has before it asks him to call: the supervisor's order and the app's own warning line in `wa-run/app.log`
   must agree (a start that ordered the switch and shows no line, or shows the line without the order, fails the step
   before he is asked anything; the supervisor also halts and stops the app if the line shows in a start that did not
   order it). In a rehearsal the fake prints the same warning line when, and only when, the variable is exactly `1`, so
   the rehearsal proves the switch reached the app of that start and of no other.
3. After his y or n about the ring the step records which handler answered and whether the TEST phone rang. When both
   handlers have a ring on record (R1's call without the flag, this one with it), the step prints the D9 QUESTION for the
   lead to put to him: accept the offer-ack as what WhatsApp Web does and write the exception into the call rule, or keep
   it suppressed (the question also says that upstream learns the caller's number-to-LID pairing from the caller's own
   claim). The tool never answers it.
4. `wa-run stop`, then start again without `--call-builtin`. The report names each start by its handler ("Call handler"
   under "What each start asked") and counts the starts that ran with the flag.

The offer-ack is upstream's own frame, not one the tool lets through, so it is not counted in the 10 and 30 of the send
budget (it is a receipt, which every client sends on a ring); that is why this start has no send in it. Upstream's own protocol
repairs of a message the tool sent (the same id sent again to one device after a retry receipt, the repair of the device list a
first send missed) do not pass the write point either and are not counted, like the offer-ack. The same handler
also learns the caller's number-to-LID pairing from the caller's own claim, a store write that stays after the start; the
report counts the call offers it was given ("Upstream answered N call offers in this start; not counted in the send
budget" under "What each start asked").

## The send run (R2)

R2 is the one run that sends. It comes after R1 (the read-only steps above) and only on the user's word, in a session in
which he said go. What it sends is small and fixed: the tool's own texts (named by an id, nobody types a text), a
thumbs-up, an edit and a delete of its own messages, at most 10 frames in one start of the app and 30 in one profile
until he resets the count. The pieces are described above ("The send steps of R2", "The peer's acts and the one resend",
"The call under upstream's handler"); this is how they fit together.

Before the start: R1 has passed on this profile (the chats `receive` and `group-receive` remembered are the ones the run
sends into), the TEST phone and the PERSONAL phone are at hand, and the budget is looked at in `wa-run state`
(`frames_used`). Never `--reset-frame-budget` or `--forget-owed-frames` without his word (the second one only after he and the
lead looked at the halts and the message's row).

The starts, each `kcap -t 900 cargo xtask wa-run start --confirmed-by-user ... --profile <name>` after his go. No digits
are typed for these starts: they are not link starts (`wa-run tail` refuses "this start is not waiting for the digits"),
and the number stays his, typed only in `wa-run tail` of a link:

1. The peer's acts, no flag needed: `step peer-react`, `step peer-edit`, `step peer-delete` (the user acts on the PERSONAL
   phone; nothing is sent).
2. A send start with `--for-send` (start A): `step resend`, call 1 (it asks his OK; the one fixed text goes and the app is
   killed at once). Then `wa-run stop` and a second `--for-send` start (start B): the app sends that message again by
   itself when its ack was not seen, and this start has counted that owed frame in its ten (`wa-run state` says "owed
   from the last start: 1").
3. In start B, `step resend` again FIRST (call 2: the same message must reach the server and show once), then the five
   send steps `step send`, `step group-send`, `step react`, `step edit`, `step delete`: 1 owed frame and 9 of their own
   are exactly the 10 a start allows, and the eleventh frame is refused by the budget. Without the resend the five steps
   are 9 of 10. Each send step asks the PERSONAL phone for one text first, because the peer writes first (below).
4. One start with `--call-builtin` for his D9, with no send in it: `step receive`, `step call` (the section above).

Then `wa-run report`: it lists every frame counted, every halt with its code word and every start's resync and handler.

What R2 must know from the fix round of W5a (`.claude/rules/whatsapp.md`, architecture §7.1):

- THE PEER WRITES FIRST in every chat that is to be sent into (F4). Kourier never starts a chat; it counts a 1:1 chat
  as started only after a REAL message of it (the peer's, or one from the phone) reached it, or after the server acked a
  message Kourier sent there. A profile that was linked and ran under W3 or W4 holds no such fact for any chat (nothing
  it kept proves a real message: a history chat row alone, a call line or a token is no start), so every 1:1 chat of it
  states "start this chat on your phone" until the peer's next message. Each send step therefore asks for ONE message
  from the personal phone into the chat it sends to, and the supervisor itself must see it. The floor of that look is
  the newest peer message the chat showed when the step took its mark (a mark on a chat that shows no row yet is
  refused); an OLDER peer message that is delivered late, after the mark, can still open the gate, because the tool
  does not compare the message's time with the start's.
- A 475 on a send (a cap on messages to new chats) is refused like a 463 and never resent (F8). The run records its
  code and the chat's state and looks at what it means; it does not retry.
- A 429 or a 500 on a send is not retried at once: the core's retry waits the floor (5 minutes, 30 seconds) and the
  frame goes out once after it (F1). The tool sees every frame of Kourier's send path in the backend's own lines: it halts
  at the first nack (the `answered` line carries the code) or when no `acked` line comes within 15 seconds, and stops the
  app. A halt cannot cancel the core's queued retry, only move it to the next send start, where it is an owed frame,
  counted before the start; the tool reports the code word, never a retry storm.
- Receipts: the first read receipt of a connection waits until the phone's read-receipt setting is known (F5).

## Stopping, keeping, undoing

- `wa-run stop` ends the app and keeps the login. It refuses while a link is in flight (between
  the phone's confirmation and "done"); `--force` overrides.
- `wa-run backup [--to <dir>]` copies the profile with the app stopped, only to an encrypted place
  outside every worktree. The copy is as sensitive as the profile.
- Undo for good: on the TEST phone remove the device in Linked devices, then
  `wa-run wipe --yes-delete-the-login`. The phone keeps the device until it is removed by hand.

## What to watch

- A warning, an "unofficial app" notice or a new-device notice on the PERSONAL phone: stop at once.
- A restricted or "needs action" account, a ban text, or a logout from the phone: the run halts by
  itself (a ban or a hold also stops the app; the login stays); do not start again before the
  user has looked at the phone.
- A start that is not online after 90 seconds: do not link again. Read `wa-run/app.log` first.

## What to send back on a failure

The step's output and `wa-run/report.md`. Never the profile directory, never a screenshot of the
phone with names on it.

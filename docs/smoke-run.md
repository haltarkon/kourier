# The smoke run against a real Signal server

This is the runbook for the first runs of Kourier against Signal's real servers, and for every
later run on the same login. Two people work together: the lead types the commands, the user
holds the phones. The tool is `cargo xtask smoke` (`xtask/src/smoke.rs`).

Kourier has passed its tests against Signal's mock server only. What the real server does
differently is listed at the end. Read the whole page once before the first run.

## The rules

- No command here contacts a real server unless it is given `--confirmed-by-user`. The lead
  passes that flag only after the user said go in the same session.
- The QR code is scanned by the test account's phone. Nothing is ever linked to the personal
  account. The personal account only sends and receives test messages.
- The login is kept. Nothing in the default run ends it. `stop` only stops the app.
- The tool sends only what a step says: the default run sends 10 messages, and a cap (30)
  stops the tool beyond that. The cap counts every send on the profile, over all runs, until
  `start --max-sends <n>` sets it anew. No step sends twice by itself, but a sending step that
  is run a second time sends a second message.
- Nothing goes to the personal account before the disappearing-messages timer of the chat was
  compared with the personal phone (`step send --timer`). A send with another timer would
  change that chat's setting on the personal account.
- Nothing the tool prints or writes names a person, a number or a message text. It is safe to
  paste into a chat or a report. The profile directory itself is not safe to share.

## What is needed

- A dedicated test Signal account on a phone (the test phone). It needs a phone number.
- The last four digits of the test account's phone number. The tool uses them to refuse any
  other account that scans the QR code. If the personal number ends in the same four digits,
  give more digits (up to 15).
- A quiet test account: no other chat should get a message during the run. `step receive`
  takes the one 1:1 chat that gets a new message as the chat with the personal account, and
  the later steps send there.
- A peer: the user's personal account on another phone (the personal phone).
- On the test phone, before the run: a 1:1 chat with the personal account that is accepted (both
  sides have written), and a group that holds both accounts and is accepted. Kourier cannot
  start a chat and refuses to send into one that is not accepted.
- For that 1:1 chat: what the personal phone shows as its disappearing-messages timer, and
  whether read receipts are on for both accounts. The tool takes the timer as `off` or in
  seconds: 30 seconds 30, 5 minutes 300, 1 hour 3600, 8 hours 28800, 1 day 86400, 1 week
  604800, 4 weeks 2419200.
- Better: a group with a third member. Kourier sends to a group with a sender key in one
  multi-recipient request only when at least two other members qualify. With the test account
  and the personal account alone the group message goes out as a 1:1 message, and the
  multi-recipient request stays untested. `step group-send` says which of the two happened.
- This machine: a checkout of the repository, a desktop session for the
  app's window, and the encrypted disk under the home directory.

## What linking changes on the test account

- One new linked device appears on the account. The phone lists it under Linked devices and
  shows Signal's own notice about a new linked device.
- The device uploads its keys: a signed prekey and a Kyber last-resort key for the ACI and the
  PNI identity, then 100 one-time prekeys of each kind per identity. While Kourier runs it
  refills them and renews the signed keys about every two days.
- The device states its capabilities. They are Signal Desktop's values.
- Kourier asks the phone for its contacts, settings and block list and reads the account's
  Storage Service records and groups. It reads only.
- From then on the device receives the account's messages and sends delivery receipts. Opening
  a chat in Kourier marks it read, which sends read receipts and a read sync.
- The server notes the device's last connection at every connect.
- Contacts' apps learn of the new device at their next send. There is no safety-number change.
- Kourier has no request that writes the profile, the avatar, the username, the PIN, a group or
  the Storage Service.

## What the run does to the personal account

- Nothing is linked to it, and nothing about it is read from Signal's servers except what any
  contact of it can read (its profile).
- It gets the test messages the steps send. In the default run that is, in the 1:1 chat, four
  texts (one of them a reply with a quote, one edited right after, one deleted right after), a
  picture, a text file and one reaction, and one text in the group: ten sends with the edit
  and the delete. Each text starts with "Kourier smoke test".
- For the messages it sends to the test account it gets delivery receipts, and read receipts
  once the chat is opened in Kourier (when both accounts have read receipts on).
- The chat's disappearing-messages timer stays as it is: every sending step compares
  Kourier's value with the one the user read from the personal phone and sends nothing when
  they differ.
- Its app learns of the test account's new device at its next send to it. No safety-number
  change and no "chat session refreshed" may appear. If one does, stop.

## Step 0: the rehearsal (no real server)

```bash
cd <the checkout of the repository>
cargo xtask smoke rehearse
```

This runs the whole tool against the mock server on a throwaway profile, offscreen, with the
mock harness as the phone. It takes about two minutes and must end with `REHEARSAL PASS`.
It proves on the mock: the profile directory is private, the lock is held, a second start is
refused, a QR code scanned by an unexpected account links nothing and the next link step
starts over, every step of the default run passes, no step sends to the peer before the
chat's timer was compared, a send with a wrong timer and a send beyond the cap are refused, a
reaction of the peer on a message of its own counts, a start after a stop reconnects without
a QR code, the three messages sent meanwhile are counted although their chat was opened and
read before the count and another chat held the newest message before the stop, a second
catch-up fails on a message Kourier cannot show (a row without content) beside one that
arrived, `step read` fails on that row as the newest one of the peer chat, a message sent
after that arrives, a start for another server is refused without a wipe, the steps that end
the login do nothing without their flags, the supervisor started by hand starts nothing, and
nothing the tool or the app wrote holds a name, a number, a text, a full id or the QR link.

If it fails, fix that first. Do not go on.

## Step 1: before the first contact

The user:

1. says go for contacting Signal in this session;
2. confirms the 1:1 chat and the group exist and are accepted on the test phone;
3. tells the lead the last four digits of the test account's number, the timer of the 1:1 chat
   as the personal phone shows it, and whether read receipts are on;
4. puts the personal phone aside until a step asks for it.

The lead checks that no Kourier is running (`cargo xtask smoke state`).

## The steps in order

Every `step` prints `PHONE:` lines with what the user does, waits for the proof, prints `PASS`
or `FAIL` with the reason, and then the app's log lines of that step. `cargo xtask smoke steps`
lists them.

A step that waits for the user waits up to five minutes (`--wait <seconds>` changes that), so
the shell call that runs it needs a timeout of ten minutes. The steps that wait for the user
(`link` and the receiving ones) send nothing. If a shell call is cut off while one of them
waits, the app keeps running and nothing is lost: run `cargo xtask smoke state`, then the same
step again. `step link` picks up a login that is already at its QR code. A sending step ends
within two minutes; when one was cut off, look at the personal phone before running it again,
because a second run sends a second message.

The first line of every step names the profile directory and the server it acts on.

### Link (first contact)

```bash
cargo xtask smoke start --env production --confirmed-by-user --expect-tail <last four digits>
cargo xtask smoke step link
```

`start` prints the profile directory, what protects it at rest, and what linking does. It
opens the app's window on the desktop. `step link` adds a Signal account in the window and
waits at the QR code.

The window needs the desktop session of the shell that runs `start`. If `start` says the app
did not come up and the log names the display, the user runs the same `start` command in a
terminal of his own. Every later command works from any shell: the app keeps running on its
own, and the commands find it through the profile directory.

The user: on the TEST phone, Signal > Settings > Linked devices > Link a new device. Scan the
code in the Kourier window and confirm. Keep the phone unlocked and online for a minute. A new
code appears every 45 seconds, six in all. If the phone offers to transfer the message
history, choose not to transfer: Kourier does not ask for it and cannot take it.

Proof: the step reports the link done and the account online. The test phone lists the device
as "Kourier smoke test" (`--device-name <text>` on `step link` gives another name).

If it fails:

- "the account guard refused the phone": the wrong phone scanned. Nothing was linked.
- 409 in the reason: the account requires a capability Kourier lacks. Stop. Do not retry.
- 411: the account has too many linked devices.
- 499 or "version": the server refuses this client. Stop.
- A timeout: look at the test phone's list of linked devices first. Remove a Kourier entry
  that should not be there.
- Never stop the app between the phone's confirmation and the end of the step. `stop` refuses
  while a link is in flight.

### Stay passive

```bash
cargo xtask smoke step passive --seconds 300
```

Five minutes without a send. The user does nothing. Proof: the account is online at every
look, and no error was reported. The step prints how many 1:1 and group chats are listed.

### Stop, back up, start again

```bash
cargo xtask smoke stop
cargo xtask smoke backup
cargo xtask smoke start --env production --confirmed-by-user
cargo xtask smoke step reconnect
```

Proof: the app comes online without a QR code, and `start` prints since when the profile is
linked and when it last connected. The test phone shows the device as recently active.

If it shows "logged out" or asks for a QR code: do not link again. Read the log first. A 401
or 403 at connect means the server dropped the device.

### Receive, read, send in the 1:1 chat

```bash
cargo xtask smoke step receive
cargo xtask smoke step read
cargo xtask smoke step send --timer <off|seconds>
```

- `step receive`. The user sends one text from the PERSONAL account to the test account.
  Proof: exactly one new message in one 1:1 chat. The tool remembers that chat as the peer
  chat, by its number, never by its name. The personal phone shows the message as delivered.
- `step read`. The lead opens the peer chat. The step first looks at the newest message from
  others there: `step receive` counts by the chat's unread counter and cannot see what the
  message shows, so a placeholder ("could not be decrypted", or a kind of message Kourier
  does not show) fails `step read`. Proof: that message is no placeholder, and the chat has no
  unread message. Kourier marks a chat read only while its window has the focus: when the
  window is behind another one, the step asks the user for the focus and waits. The user
  clicks the window's title bar (or an empty spot of the open chat's message area), not a
  chat in the list: that click would open another chat (the step then opens the peer chat
  again). The step prints the chat's disappearing-messages timer as Kourier knows it. Compare
  it with the personal phone now. The personal phone shows the message as read when read
  receipts are on.
- `step send`. Give `--timer` the value the PERSONAL phone shows for this chat. The step sends
  nothing when Kourier's value differs: every 1:1 message restates the timer, and a wrong one
  would change the chat's setting on the personal account. Proof: the message is sent. The
  personal phone gets it once, with no timer change and no system message. The test phone
  shows it as sent.

`step send` is the step that confirms the timer. Every later step that sends to the personal
account (`attachment-send`, `react`, `reply`, `edit`, `delete`) checks that Kourier still shows
the confirmed value and sends nothing otherwise. Run before `step send`, or after the timer
changed, such a step fails with what to do: compare with the personal phone again and give
`--timer <off|seconds>` to that step.

If a send answers with a challenge (428): stop the sending steps. The window shows "Verify to
Keep Sending". The user presses "Open in Browser" and solves the captcha there. The page ends
on a link that starts with `signalcaptcha://`. Do not let the browser open it (on this machine
it would go to Signal Desktop): copy the link, paste it into the field of Kourier's dialog and
press "Submit". The message that waited goes out then; it was counted already, so do not run
its step again. Do not let sends run against a standing challenge. If in doubt, `stop`: the
message stays in Kourier's queue.

A message that did not go out (no connection, a challenge) is not lost and is not sent twice:
it waits in Kourier's queue and goes out when the connection is back, also after a `stop` and
a `start`. Running its step again would send a second message.

### The group

```bash
cargo xtask smoke step group-receive
cargo xtask smoke step group-send
```

- `step group-receive`. The user sends one text in the group from the PERSONAL account. Proof:
  one new message in one group chat, remembered as the group chat.
- `step group-send`. One text from Kourier. Proof: sent, and it arrives once on every device.
  The log lines of the step name the route of the multi-recipient send. If it fails with 401
  or a server error, note the route and stop sending to the group.

### Attachments

```bash
cargo xtask smoke step attachment-receive
cargo xtask smoke step attachment-send
```

- `step attachment-receive`. The user sends one photo from the PERSONAL account. Proof: a new
  message with a picture that Kourier downloaded and shows. The picture is told by its
  message: an older picture in the chat neither counts nor hides it. The step sees it only
  while the peer chat is the open one: leave the chat list alone while it waits. When another
  chat was opened, the step asks for the peer chat and goes on once it is open again.
- `step attachment-send`. Kourier sends a small generated picture and a small text file, two
  messages. Proof: both sent, and both open on the personal phone. If an upload fails, the log
  lines name the CDN and the status. If the step fails after it put its two files into the
  composer, they stay there, and every later sending step refuses to send until the user
  removes them in the window (the x on each file): otherwise they would go out with the next
  message.

### Modifiers

```bash
cargo xtask smoke step react
cargo xtask smoke step reply
cargo xtask smoke step edit
cargo xtask smoke step delete
cargo xtask smoke step modifiers-receive
```

- `step react`: a thumbs-up on the peer's newest message.
- `step reply`: a reply that quotes it.
- `step edit`: a text, then its edit.
- `step delete`: a text, then its delete for everyone.
- `step modifiers-receive`. The user, from the PERSONAL account in the chat with the test
  account: reacts to one of the newest messages, sends a reply that quotes a message, sends a
  text and edits it, sends a text and deletes it for everyone. Proof: Kourier shows all four,
  each of them new since the step began. The step tells marks apart by their message, and a
  reaction by its emoji too: what an earlier attempt left in the chat neither counts nor
  hides a new mark. Signal keeps one reaction per person and message, so on a message that
  already carries the user's reaction he picks another emoji. Like `attachment-receive`, the
  step sees the chat only while it is the open one. With a disappearing timer of seconds in
  this chat Kourier's own messages are gone before the modifier steps find them: use
  minutes, or none.

Leave disappearing timers alone unless the user wants that test.

### Offline catch-up

```bash
cargo xtask smoke step offline-begin        # hands off the Kourier window while it runs
cargo xtask smoke stop
# the user sends three texts from the PERSONAL account: two in the 1:1 chat, one in the group
cargo xtask smoke start --env production --confirmed-by-user
cargo xtask smoke step offline-catch-up --expect 3      # hands off the window again
```

Run `offline-begin` and `stop` back to back, and ask for the texts only after the stop: a
message that arrives between the two counts as sent while offline. The user sends exactly the
three and nothing else until the catch-up step has ended: a text typed after the start counts
too, and "more than expected" fails the step.

Hands off the Kourier window while `offline-begin` and `offline-catch-up` run. Both open every
1:1 and group chat in turn to read its stored messages: `offline-begin` once, `offline-catch-up`
again and again until the count is there (at least twice, for up to `--wait` seconds, 120
unless given). A click on a chat while a step reads fails that step ("was left while its rows
were read", or "did not open"). What is typed meanwhile goes to whichever chat is open at
that moment, and a reply or an edit in progress is dropped at each switch. Each of the two
steps asks for the hands off in its first `PHONE:` line. Between the `start` and
`step offline-catch-up` the user may open chats in the window: the step counts stored
messages, not unread counters. A failed `offline-catch-up` can be run again as long as the
user has sent nothing more: the baseline stays until the step passes.

Both steps mark chats read. Opening a chat marks it read when the Kourier window has the
focus, so with a focused window each of the two steps marks every 1:1 and group chat of the
test account read, not only the two chats of the run: read receipts go to the authors of the
unread messages (when the account has read receipts on), a read sync goes to the test phone,
and the disappearing timers of those messages start. A third member of the group, or any
contact with an unread message, gets a read receipt from a step that sends no message. While
another window has the focus (the terminal), nothing is marked read. The steps record this
in a `NOTE:` line.

The timer advice of the modifier steps holds here too: use a disappearing timer of minutes,
or none, in the chats the texts go to. With a timer of seconds a text that the first look
marked read is erased before the last look, and the step finds fewer than arrived. For the
same reason run `offline-catch-up` soon after the `start`: a text that was read, in the
window or on the test phone, is erased when its chat's timer ends, and the step cannot count
it then.

Proof: three new messages from others, each once and each with its content (a text or an
attachment). The step prints them per chat. Check their order by eye in the window.

A new row without content fails the step, whatever the count: it is a placeholder (a message
that could not be decrypted, or of a kind Kourier does not show) or a message that was
deleted for everyone, never an arrived message. Read the app's log from the start on for the
decryption error or the retry job; the step's own log lines begin after the start. A row
without content that an earlier look of the step showed and the last look does not (a message
that got its content with a second delivery) does not fail the step. The step says so in a
`NOTE:` line, and it is a finding all the same.

The count has a limit. One look at a chat shows its newest page: 50 rows, timer and group
changes included, and the tool sees the messages among them. When a chat shows 40 or more
messages (the account's own included) and none of them is from before `offline-begin`, it
may hold more new messages than the look shows: the step then fails with "may hold more new
messages than the tool can count" and gives no number. Count such a catch-up by eye.
Up to 39 new messages in one chat are counted, as long as that chat got no more than ten
timer or group changes in the same time.

`offline-catch-up` takes only a baseline that this version of the tool wrote. With one from an
older version it says so: run `offline-begin`, stop, send and start again.

### End of the run

```bash
cargo xtask smoke stop
cargo xtask smoke backup
cargo xtask smoke report
```

The device stays linked. `report` writes `smoke/report.md` into the profile directory: the
steps in order with their results, times and notes.

## How to stop at any point

`cargo xtask smoke stop` asks the app to quit and waits for it. It ends nothing but the
process. It refuses while a link is in flight. If the app does not stop within a minute it is
left running and the command says so; `stop --force` then ends it with SIGTERM.

Closing the window also quits the app. Do not use "Remove account" or "Link again" in the
window during the default run: Remove deletes the device on the server and shreds the keys.

`cargo xtask smoke state` shows the profile and the running app at any time.

## How the login is kept

- The profile lives in `~/.local/share/kourier-smoke/<name>` (`$XDG_DATA_HOME/kourier-smoke`
  when that variable is set). The name is `default` unless `--profile <name>` says otherwise.
  It is outside the repository and its build directories, and it is not the app's normal data
  directory. `git clean`, `cargo clean`, the demo, the perf run and the tests never reach it.
- The directory is the whole login. The app gets it as `KOURIER_HOME` with the development
  keyring, so the desktop's keyring (KWallet) is never read or written, and no other profile's
  key can be touched.
- At rest the data is protected by the disk encryption below the home directory (LUKS; `start`
  checks it and refuses a plain disk) and by the directory's mode 0700. The databases are
  SQLCipher files, but their key comes from a passphrase that is a public constant in the
  source (`crates/kourier-app/src/dev.rs`). So the databases' own encryption protects nothing
  here. Never copy the profile or a backup to `/media/...` or any other plain disk.
- One app per profile. The supervisor holds a lock for the app's whole life, and the app holds
  its own lock on its data directory. A second start is refused.
- A profile belongs to the server it was made for. `start` refuses another `--env`, and the
  app itself refuses to connect an account to another server than the one that linked it. A
  wrong server would answer with "not registered", and that would wipe the login.
- A start on a linked profile needs no QR code. It prints since when the profile is linked and
  when it last connected.
- Signal removes a linked device that has not connected for a while. Signal's support text
  says 30 days. The number is not in Signal Desktop's sources, and the server's own constant
  could not be read offline. The tool treats 30 days as the deadline and warns from day 21.
  Connecting for a minute resets the clock: `start`, wait for "online", `stop`.
- A newer build migrates the databases forward, and an older build then refuses the profile.
  `start` notices a schema change and asks for `--allow-schema-change`. Take a backup first.
- A debug build that crashes leaves no core dump: the tool makes the app non-dumpable.

What must persist is the whole directory. The parts that matter most:
`kourier/data/core.db`, `kourier/data/accounts/<uuid>/` (`key.wrap`, `backend.db` with its
`-wal` and `-shm`, `blobs/`), `kourier/config/dev-keyring.json`, and `smoke/state.json`.

## Backing up

```bash
cargo xtask smoke stop
cargo xtask smoke backup            # or: --to <a directory on the encrypted disk>
```

The app must be stopped; `backup` refuses otherwise. It copies the whole directory next to the
profile as `<name>.backup-<date>-<time>`, mode 0700. The copy holds the test account's keys. It
is as sensitive as the profile.

Restoring is a last resort. A copy older than the last run brings back old ratchet states, and
the peers may then see a session reset. Restore only a copy taken after the last run, as a
whole directory, to the same path, and never run the original and a copy at the same time. If
in doubt, unlink on the phone and link again.

To restore, with the app stopped (`<profiles>` is `~/.local/share/kourier-smoke`):

```bash
mv <profiles>/default <profiles>/default.set-aside
cp -a <profiles>/default.backup-<date>-<time> <profiles>/default
```

Then `start` as usual. It says that the peer chat and the group were forgotten (a restored
database may number its chats differently): run `step receive` and `step group-receive` again
before any sending step. Delete the set-aside directory once the restored profile works; it
holds keys too. The one case where a restore is the right answer: Kourier wiped its keys by
itself while the phone still lists the device (see "What to watch": a refused PNI prekey
upload). The copy from the end of the last run then still matches the device on the server.

## Undo for good

1. On the test phone: Settings > Linked devices > the Kourier entry > Unlink. The server
   deletes the device, its prekeys and its message queue.
2. `cargo xtask smoke stop`
3. `cargo xtask smoke wipe --yes-delete-the-login` deletes the profile directory. Without the
   flag it only says what it would delete.
4. Delete the backups by hand: the `<name>.backup-*` directories next to the profile.

With the development keyring nothing else exists anywhere. Messages already delivered stay on
the other devices. Uploaded attachments stay on the CDN until Signal expires them.

If the wrong account was linked: unlink that device on that phone at once, stop Kourier and
wipe the profile.

## Opt-in steps

These end or risk the kept login. They are never part of the default run, and each needs the
user's say.

- `cargo xtask smoke step unlink-from-phone --destroys-login`. The user unlinks the device on
  the test phone. Proof: the app reports logged out and wipes its keys. The log shows what the
  server answered. Without the flag the step does nothing and says so.
- `cargo xtask smoke wipe --yes-delete-the-login`: see above.
- "Remove account" and "Link again" have no tool step. They are done by hand in the window,
  and only on purpose.

## A link with the history transfer (experimental)

The transfer of the account's history at link time is switched off in the app until the
import is whole (architecture §14.2 item 14). A run can switch it on: `start
--history-transfer` (the tool sets `KOURIER_HISTORY_TRANSFER=1` for the app; a variable
inherited from the shell has no say). It matters only for a FIRST link, so it needs a
profile without an account. Use a profile of its own and leave the kept `default` login
alone:

```bash
cargo xtask smoke start --profile transfer --env production --confirmed-by-user \
    --expect-tail <last four digits> --history-transfer
cargo xtask smoke step link --profile transfer --wait 1800
```

- The test phone then holds two Kourier devices, the kept login and this one. Remove the
  new one on the phone when the rehearsal is over, or keep the profile for another run.
- After the scan the phone asks whether to transfer the message history. Choose to
  transfer. The link page shows "Waiting for Your Phone", then the download, then
  "Importing Messages". The login ends after the transfer ends, which is why the step gets
  a long `--wait`.
- The import brings people, 1:1 chats and plain text today. Everything else is counted
  and skipped.
- What to read in the log afterwards, counts and times only (the log holds no content):
  the line "device linked" with `envelope_to_linked_ms` (the phone waits 60 seconds for
  the link), the lines of the transfer's states, the import's summary (frames by kind,
  rows staged, rows skipped by reason) and the delivery's counts.
- The other steps work on that profile as on the default one, with `--profile transfer`
  on each command.

## What to watch during the run

- A log line "post-connect steps failed": the prekey upload was refused. The login then stays
  on "Syncing".
- A log line "typed request transports from the remote config": which of the group send and
  the upload form go over gRPC.
- Any "Undecryptable" placeholder or retry job in the log is a finding to stop on.
  `offline-catch-up` fails on a new row without content. `receive` and `group-receive` count
  by unread counters and do not look at the content: a "could not be decrypted" row in the
  window after one of them is the same finding. `step read` fails on one as the newest
  message of the peer chat; nothing looks at the group chat's rows after `group-receive`.
- On the personal phone: each message once and in order, no "chat session refreshed", no
  safety-number change.
- 413 and 429: check that the Retry-After is waited out.
- Key maintenance runs at every later start and two days after the link. A 403 or 422 on the
  PNI prekey upload wipes the login locally, as Signal Desktop does. The device stays on the
  server then: do not link again, read the log, and see "Backing up" for the restore.
- A sending step that fails with "the timer of this chat is … in Kourier now": someone changed
  the chat's disappearing-messages timer, or Kourier learned another value. Compare with the
  personal phone before anything else is sent.
- A banner on the test phone that asks to open Signal on the phone concerns the test phone
  itself: open Signal there regularly.

## When a step fails: what to send back

1. The step's whole output: the `FAIL` line with its reason, the `NOTE:` lines and the log
   lines between the two `--- log of this step ---` markers. They are redacted.
2. The output of `cargo xtask smoke state`.
3. `smoke/report.md` after `cargo xtask smoke report`.
4. What the phones show: the list of linked devices on the test phone, and the chat on the
   personal phone. Describe it in words. Do not send screenshots of chats.

Do not send files from the profile directory other than `smoke/report.md`, `smoke/app.log`,
`smoke/events.jsonl` and `smoke/steps.jsonl`. The databases and the keyring file are the login.

After a failure, do not repeat a sending step until the reason is understood. Stop the app if
in doubt. The login is kept.

## Running the tool against the mock by hand

```bash
cargo xtask smoke start --env local:<handshake.json>     # throwaway directory, offscreen
cargo xtask smoke step link --mock
cargo xtask smoke stop --mock
cargo xtask smoke wipe --mock --yes-delete-the-login
```

A local environment uses a throwaway directory below the target directory and never a kept
profile: `--env local:…` with `--profile` is refused. Every later command needs `--mock`
(or the same `--home`). Without it a command means the kept `default` profile; every step
prints the profile and the server it acts on in its first line. `cargo xtask demo
--interactive` starts a mock world with a phone to play with.

## What the mock could not show

These are open until the first real run answers them:

- whether the server accepts the user agent `Kourier/<version> libsignal/<version>` at all,
  and which remote config it serves for it;
- which capabilities the server takes from a linked device, and what the link request must
  carry today to avoid 409;
- what the server answers for a removed device, and after how long it removes an idle one;
- whether the phone links from a QR code without offering a history transfer;
- the real answers of the key uploads, of the multi-recipient send, and of a challenge;
- the CDN's behaviour for uploads, resumes and range requests;
- server timestamps that differ from the client's, and a catch-up after days offline;
- what Signal's own apps show for Kourier's messages.

# The Telegram run against the real service (R0)

This is the runbook for the first run of Kourier's Telegram backend against Telegram's servers.
Two people work together: the lead types the commands, the user holds the files only he may write.
The tool is `cargo xtask tg-run` (`xtask/src/tg_run/`). It is the sibling of the smoke tool
(`docs/smoke-run.md`) and the WhatsApp tool (`docs/whatsapp-run.md`), made for Telegram's rules.

R0 is the smallest run there is. The app asks Telegram's TEST servers, through Kourier's own gate,
for ONE login QR code that nobody scans, and quits. No account can result. Nothing is sent, nothing
is linked, nothing is registered. Later runs (R1 and on: a login by QR code or by phone, history,
receive, read) come with the next half of the tool and get their own chapters then.

## The rules

- No command here contacts Telegram unless it is `connect --confirmed-by-user`. The lead passes
  that flag only after the user said go in the same session. A go in an earlier session counts for
  nothing. Tests, demos, rehearsals and agents use the fake that plays Telegram. The rehearsal runs
  the whole tool against it inside `unshare -rn`, with no network at all.
- A real run is never a test and never part of `ci`. `connect` refuses inside a test run (the test
  lock, nextest), outside the `kourier.slice` CPU cap (a build container does NOT count for it), on
  a tree that is not clean, on a HEAD that is not `main`, and on a disk without encryption unless
  the user accepted that (`--allow-unencrypted-disk`).
- `--env test` only. Telegram's production servers come with R2: the tool says so and starts
  nothing.
- The app runs OFFSCREEN. Nobody sees the QR code, no event or log carries its link
  (`KOURIER_DEV_QR_URL=0`), and the app is given a number tail that no phone number ends in
  (fifteen zeros), so even a scanned code could not make an account. The tool changes the app's
  page right after the code appears, and the app cancels the login when its page is left. Four
  belts, all of them on. If the app does print a link, the walk FAILS.
- The user's API file is HIS. He writes it himself (below). The tool checks its owner, mode and
  size and never opens, copies, prints or deletes it. No API value, hash, nonce, token, number,
  code or password is ever on a command line, in a report or in a log: the parser refuses a flag
  whose name could carry one, without echoing it.
- The tool prints and writes counts, words and times only. It never prints a path (the profile's
  NAME only), a number, a name, a text, a link, an id, a nonce or a hash of it.
- The gate keeps its own wall besides the confirmation: even a confirmed dial goes only to the
  address blocks of `crates/kourier-telegram/src/gate/ranges.rs` and only to ports 443, 80 and
  5222.

### What the confirmation is, and what it is not

`connect --confirmed-by-user` makes ONE confirmation for ONE start of the app. The tool makes a
random nonce (32 bytes from the system, held in its own memory, in the environment of the app it
starts, and for a moment in the input of the `sha256sum` process it starts to hash it; a process
of the same user could read it there, and such a process could forge the whole confirmation
anyway). It writes the nonce's SHA-256 with the time and the servers into `tg-run/dial-go.json` of
the profile (0600, an atomic rename, right before the app starts) and hands the app the nonce.

The app opens the file without following links, deletes it at once, and lets the gate dial only
when the file is fresh (two minutes), its fields are exact, it had no other name, its hash is the
nonce's, the servers are the test servers, and a number tail was given. A start without all of it
keeps the gate's old behaviour: every dial is refused. The tool makes itself non-dumpable before it
makes the nonce. It starts the app directly, never through `kcap`, `env` or a shell, each of which
would be a process with the nonce in its environment, as an execute-only copy. It removes a
leftover file at the start, after a failed start and at the end, and it looks at the file once
more, before its own removal, to say who took it.

This stops accidents, stale files and replays. It does NOT stop a process of the same user that
forges the whole file and variable: such a process could do anything the tool does. The user's go
is the rest. That is the same threat model as `wa-run`'s typed digits.

## What is needed

1. THE API FILE, written by the user himself. The pair comes from the user's own application on
   Telegram's developer page. Run once, in a terminal of his own:

       install -d -m 0700 ~/.local/share/kourier-tg-run
       install -m 0600 /dev/null ~/.local/share/kourier-tg-run/telegram-api

   then open the file in an editor and write two lines, `id=<the digits>` and
   `hash=<32 lower-case hex digits>`. Nothing goes in the shell history (do not `echo` the
   values). The tool refuses a missing file, one that is not private (0600), not his, a link, a
   directory, a pipe, one with another name, an empty one and one longer than 256 bytes, and says
   which. The app reads the file only in a debug build inside a kept run profile.
2. THE LIBRARY, fetched from the server: the pinned TDLib with the lane's patches is built there,
   not on this machine: `srv -f target/tdlib 'kcap -t 900 cargo xtask tdlib build'` from the
   checkout. `connect` checks the library (the file, the genuine-keys row, the manifest's patch
   list against `third_party/tdlib/tdlib.pin`) and remembers the sum of the very bytes it checked.
   It stages a read-only copy of the library and refuses unless the copy has that sum, so the
   library that runs is the library that was checked. It never takes a library named by
   `KOURIER_TDJSON`, and it refuses that variable, and `KOURIER_TG_API_ID` and
   `KOURIER_TG_API_HASH`, when they are set in its environment.
3. THE ADDRESS LIST, verified with the user's go. `crates/kourier-telegram/src/gate/ranges.rs` is
   the only file that names Telegram's address blocks, and it says UNVERIFIED until Telegram's
   published list was fetched. With his go, the lead fetches the list (the fetch command is given
   to him at that point, so his go is one answer), compares it with the two blocks, fixes them,
   and writes ONE marker line, replacing the UNVERIFIED paragraph (the word must stand nowhere in
   the file):

       //! VERIFIED <yyyy-mm-dd> sha256:<64 hex of what was fetched> list-sha256:<64 hex> <source>

   `cargo xtask tg-run ranges` prints the `list-sha256:` value for the working tree (it opens
   nothing else and contacts nothing): the hash of the text of the two blocks `TELEGRAM_V4` and
   `TELEGRAM_V6`. `connect` reads the file as it stands at HEAD, so the marker is committed first,
   and refuses unless exactly one such line stands and its `list-sha256` is the recomputed one: an
   edit of a block after the marker is refused. `--allow-unverified-ranges` passes the check with a
   warning. It is the user's word only, never the lead's.
4. THE PIN BUMP (LC-20) on `main`: R0 runs after the pinned TDLib was raised, so that the first
   real contact is made by the library that will be used. The R0 profile is created after LC-20 lands,
   never before: a `tdlib/` directory moves one way (an older pin never opens what a newer one wrote).
5. THIS MACHINE: a checkout with a clean tree whose HEAD is `main`, the profile on an encrypted
   disk (the profile's database key is a public constant, so on a plain disk the keys lie there
   readable), and `kcap`, the CPU cap. Every start goes through it.

## Step 0: the rehearsal

`srv 'kcap -t 5400 cargo xtask tg-run rehearse'` from the checkout, on the agents server. It takes
the test lock, builds the app once, stamps it into a throwaway place below `target/tgr/`, and runs
the tool's own commands as separate processes, each in `unshare -rn` (no network interface but
`lo`), offscreen and silent, against the fake that plays Telegram
(`KOURIER_DEV_REHEARSAL=telegram`). Its checks, in order:

1. `connect` without the user's go on a scratch keeper root is refused and makes nothing;
2. a connect whose order is NOT confirmed: the app says `mode=refusing reason=no-confirmation`,
   the walk saw no confirmation file (`confirmation: none written`) and handed over no nonce, the
   verdict is PASS, the gate's counts say "not measured (rehearsal)" and the preflight says
   `disk not checked`;
3. a CONFIRMED connect: `mode=telegram reason=confirmed`, the walk saw the file gone before its
   own removal (`confirmation: taken by the app`), and the launch line names
   `KOURIER_DEV_TELEGRAM_DIAL` and no value;
4. in both walks nothing of the real backend ran: no gate-counts line, no lock-down line, no line
   about the helper, no API file, no helper and no library staged;
5. the staged app's probe registers Telegram with the fake's switch and does not without it;
6. in each walk the first QR step came, no step after it, no account, the page change was
   answered, and the state file keeps the two verdicts;
7. `wipe` without its flag deletes nothing; with `--yes-delete-the-login` it deletes the profile
   and leaves the keeper root;
8. the leak search over every file the walks wrote (the stamped program aside), over everything
   the commands printed, and over the app's RAW stderr (each walk counted the needles in every
   line before the redaction: `raw needles in the app's stderr: 0`) finds none of the fake's 18
   constants (names, numbers, ids, tails, texts, code, password, hint, e-mail), the confirmed
   walk's nonce, its hash, or the sentinel tail.

It ends with `REHEARSAL PASS: <N> checks in <S> s` and the app's memory at the end. S counts from
the start of the command, the wait for the test lock included; on a quiet machine the walks take
less than a minute after the build. `--keep` leaves the place and says its size; `--no-build`
skips the build (the debug app must be there).

What it cannot show: the warning page (the fake has no risk notice, so the rehearsal proves that
the link page opens from the profile's one-shot switch and the QR flow starts; the real
descriptor's warning is acknowledged by the user's go for R0, and R0 makes no account), the real
gate and helper (the lock-down word, the counts), Telegram's own answers, the cap on the size of
`app.log`, and the app's death with the tool (`PR_SET_PDEATHSIG`). A real run also cannot show a gate
that was dropped without its stop line (a panic) when a later gate of the account stopped cleanly.

## R0: the connect

1. Build FIRST, with no go: the build contacts nothing. On this machine:

       kcap -t 5400 cargo build -p kourier -p kourier-tdlib

   This is the build the tool would make anyway, so the tool's own build in the next step is
   incremental.
2. The lead asks the user for his go. Only then, on this machine:

       kcap -t 1800 cargo xtask tg-run connect --confirmed-by-user --env test

   (`--profile <name>` for another profile than `r0`; `--no-build` reuses the profile's stamped
   copies, which must match HEAD and their sums, and for a real run the stamped library must be
   the one the preflight just checked.)
3. The preflight prints what it found (words only: the profile's name, `disk encrypted`, the
   build, `address list verified`, `library checked`) and starts nothing if anything fails. Then
   the tool builds `cargo build -p kourier -p kourier-tdlib` itself, copies the app and the helper
   (execute-only) and the library (read-only) into the profile with a stamp of the commit and the
   three sums, writes the profile's switches (`networks-on = ["Telegram"]` and the one-shot
   `pending-link`, which makes the app open the link page by itself, offscreen) and starts the
   staged app a first time, to ask which networks it would register: Telegram must be among them.
4. It writes the confirmation and starts the app a second time. It waits at most 180 s for the
   first login QR code, changes the app's page to the list (which cancels the login), reads the
   app's state, asks it to quit and waits at most 40 s (then kills it). Each answer has 20 s. A
   confirmed connect with no QR code within its bound FAILS.
5. It prints the verdict and writes `tg-run/report.md` in the profile.

### What PASS means

ALL of these. A line that is missing FAILS ("not measured" is never a pass in a real run):

- the dialer line says `mode=telegram reason=confirmed`, once;
- `confirmation: taken by the app`: the app took the file itself (a file that still stands after
  the walk fails the run);
- the app SHOWED a login QR code: a login step `qr` of the kind `qr`, followed by the event that
  says the code was shown (`QR codes shown`: at least 1). A login that failed at any step fails the run,
  the QR step included, and so does any login or account error the app reported, or a QR link on
  the app's output;
- no login step after the first shown code. Another shown `qr` step, a refreshed code, is allowed;
- the page change was answered, and the app's state lists no account after it;
- the app quit by itself with status 0;
- exactly ONE gate session: the gate's FINAL counts line (the one told at its stop; a periodic
  line is a partial count of the same gate and is never added) is there once. More than one means
  the helper was started again, and fails the run;
- EVERY lock-down word is `on`, and every account actor that told its lock-down word has a final
  counts line;
- over the final counts line, `dials >= 1`, `dial_failures < dials`, `rule == 0`, `port == 0`,
  `address_type == 0`.

### What to watch

- A lock-down word other than `on`: stop. The helper is not locked to the gate. Do not run again
  until the lane has looked.
- A non-zero `rule` count ("the gate refused N connections by its address list: the list is
  stale"): stop and fix the list (`ranges.rs`), then verify it again with the user's go. A non-zero
  `port` or `address_type` count is a finding of the same weight.
- The report says `sends: not measured`. R0 makes no account and the tool sends nothing, but
  nothing counts it, so the report never claims a zero. Nothing in a PASS says more.

### What to send back

The lines `connect` printed and `tg-run/report.md`: the profile's name, the servers, the build
commit and the walk's counts, words and times (no value, name or number). Never the profile
directory, never `app.log` unread, never a screenshot with names.

## Stopping, keeping, undoing

- The tool starts the app twice (the probe, then the walk) and ends both; there is nothing to
  stop. A test account is not touched (R0 makes none). One `connect` runs per profile at a time.
- `cargo xtask tg-run wipe --yes-delete-the-login [--profile <name>]` deletes the profile directory
  for good (the app's data, the stamped copies, the logs). Without the flag it deletes nothing and
  says so. The user's API file is in the keeper root beside the profiles and is never touched by
  it. No profile may be named after it, and `wipe` refuses a name that is no profile name.

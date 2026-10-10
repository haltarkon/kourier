# Contributing to Kourier

Thank you for looking at the code. Bug reports, small fixes and larger changes are all welcome. For
a larger change, open an issue first and say what you plan: the design is in
`docs/architecture.md`, and a change that contradicts it needs a talk before the work.

TODO(maintainer): say where issues and pull requests go once the project is hosted (this file
assumes a hosting site with issues and pull requests; the templates in `.github/` are for GitHub).

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md). A security problem
does not go into a public issue: see [SECURITY.md](SECURITY.md).

## Sign your commits (DCO)

Kourier takes contributions under the [Developer Certificate of Origin
1.1](https://developercertificate.org/) and does not ask for a copyright assignment or a CLA. You
keep your copyright; your contribution comes in under the license of the files it changes
(AGPL-3.0-only for code, see `REUSE.toml`).

Every commit needs a `Signed-off-by` line with your real name and an address you can be reached at:

```bash
git commit -s
# Signed-off-by: Your Name <you@example.org>
```

In plain words, the sign-off says: you wrote the change or have the right to submit it under the
project's license, and you understand that it and your name are kept in the public history. The
linked text is the authority, not this summary. A pull request with an unsigned commit is asked to
fix it (`git rebase --signoff main`).

If an AI tool helped write a contribution, read [AI-USE.md](AI-USE.md): you sign off for every line
you submit, so you must have read and understood it.

## Build and test

How to build is in [README.md](README.md). The commands you will use:

```bash
cargo build                          # whole workspace; binary in target/debug/kourier
cargo xtask ci                       # what a change must pass
cargo test -p kourier-core           # one crate while you work
QT_QPA_PLATFORM=offscreen cargo test -p kourier-qt
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo xtask ci` runs formatting, clippy with warnings denied, the check of allowed crate
dependencies (`cargo xtask deps-check`), all tests, `qmllint` and a comparison of the user
interface's screenshots with stored ones. Without picture changes it takes about nine minutes;
with them about twenty.

- The tests of the Signal backend (`kourier-signal`, `kourier-e2e`) run against Signal's own mock
  server through `tests/mock-harness/` and need Node.js 24 (`npm ci` once in that directory). Run
  them with `-- --test-threads 2`. `KOURIER_REQUIRE_MOCK_HARNESS=1` turns a skipped harness test
  into a failure; use it before you send a change that touches the backend.
- `cargo xtask shots` renders the user interface headless and compares each picture with the one
  you rendered before (`target/shots-baseline`, local: pixels depend on your fonts, Qt and Breeze).
  It names the pictures that changed and writes side-by-side images to `target/shots/diff/`; look
  at those, then `cargo xtask shots --accept` if the change is meant. A new fixture page goes into
  `PAGES` (`crates/kourier-qt/src/lib.rs`) and `SHOT_PAGES` (`xtask/src/main.rs`); a test checks
  that they agree.
- A build tree with Qt and libsignal takes 15 to 40 GB. Check `df -h` before a build and delete
  old target directories.
- `cargo xtask install --user` registers Kourier for `sgnl://` and `signalcaptcha://` links and can
  take them from Signal Desktop. While testing, install into a temporary prefix
  (`cargo xtask install --prefix <dir>`).

## Rules that matter

These are enforced by review and, where noted, by tests and `cargo xtask`:

- **Tests never touch a real Signal account.** Tests run against the fake network and Signal's mock
  server. Nothing in the test suites, in a demo or in a performance run contacts Signal's servers.
  `cargo xtask smoke` is a guided run of the real application against Signal's servers with a
  dedicated test account; it is the maintainer's tool, it needs the maintainer's explicit go for
  every session, and it is described in `docs/smoke-run.md`. If you must try a development build
  against Signal's servers, use a dedicated test account on a spare number, never your everyday
  account, and never in an automated test.
- **Follow the reference apps for the protocol, not for their limits.** A limit of Signal Desktop
  (or another reference client) is kept only when the server or the other clients enforce it, or
  when it protects interop, privacy or security; say which in a comment. A limit that is only that
  app's own interface choice is not copied. Where the wire format cannot carry what the user asked
  for in one message, adapt (split, convert, queue) instead of refusing. A new limit gets a row in
  the table in `docs/architecture.md` section 13, phase H.
- **Never log message bodies, names, numbers, keys or tokens.** Logs pass a redaction filter, and
  a test searches captured logs for fixture names and texts.
- **Nothing is acknowledged to a server before it is stored durably**, and the core's ingest is
  idempotent (section 5.6 of the design).
- **The Qt thread never blocks and never touches a database.** `block_on` is banned in `kourier-qt`
  by clippy.
- **No protocol types in public items outside a backend crate**, and the crate edges in
  `cargo xtask deps-check` hold. Add a crate or an edge there and in the design document together.
- **Text from the network or from a contact never reaches a QML label that guesses its format.**
  Use `Style.plain()` or `textFormat: Text.PlainText` (design section 9.4).
- **Keep `docs/architecture.md` true.** A change that alters a decision changes the document in
  the same pull request.

## Commits and pull requests

- One topic per commit. The subject is `Area: what changed`: a short area before the colon
  (`Signal`, `Core`, `UI`, `docs`, `cargo xtask smoke`), then a plain description in the present
  tense, no final period, about 72 characters at most. The body says why, not what the diff already
  shows. The project does not use Conventional Commits; the changelog groups by the area before the
  colon.
- Add or change a test with every behavior change. A fix for a bug comes with a test that fails
  without it.
- Keep a pull request to one subject. Describe what you ran: at least `cargo xtask ci`, and for a
  change that draws something, the pictures that changed.
- Do not reformat or move code you do not otherwise change.
- A user-visible string needs a context and goes into the catalog: run `cargo xtask i18n extract`
  after adding or changing one. Translating is described in [TRANSLATING.md](TRANSLATING.md).

## Reporting a bug

Use the issue template. Say which version and how you installed it, your distribution, Plasma, Qt
and Kirigami versions, and Wayland or X11. Logs are in `$XDG_STATE_HOME/kourier/logs/` (usually
`~/.local/state/kourier/logs/`) and are redacted, but read them before you paste them: a phone
number, a name or a message text in a log is a bug worth its own report (without the content).

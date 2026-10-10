# How AI is used in Kourier

Most of the code in this repository was written by an AI coding agent. This page says what that
means, how the result is checked, and what has not been done. It is written for people who decide
whether to trust, package or contribute to the project.

## What is AI-written

Kourier is developed with [Claude Code](https://claude.com/claude-code), Anthropic's coding agent,
under the direction of the maintainer.

- **The maintainer decides.** The goals, the scope, the choices between options (storage engine,
  libraries, which limits to keep, how the interface behaves, what is left out) and the rules in
  `CLAUDE.md` and in the design are the maintainer's. Many of them are recorded as "user decision"
  entries with a date in `docs/architecture.md`.
- **The agent writes.** The design document, the Rust and C++ code, the QML interface, the tests,
  the build tooling (`xtask`) and the documentation, including this page and the other repository
  files, were drafted by the agent. Some implementation work is handed to sub-agents running
  smaller Claude models, in time-boxed tasks with a written brief. The session that committed
  reviews their changes first.
- **Third-party material is not AI-written.** The five Signal `.proto` files, the Signal
  certificate authority file and one test vector are copied from Signal's published source with
  their original headers (`REUSE.toml` lists them). The Contributor Covenant text is the
  original.
- **Commits say so.** Every commit so far (61 between 2026-10-05 and 2026-10-08) carries a
  `Co-Authored-By: Claude …` trailer naming the model of the session that committed. The commit
  author is the maintainer, who is responsible for what is committed.

TODO(maintainer): say in your own words how much of the code you read yourself and what you
check by hand. This page cannot know it, and it is the sentence readers will look for.

## How the result is checked

What exists in the repository today:

- **Automated tests.** About 1,260 tests ran in the last full run on 2026-10-08, from
  `cargo xtask ci`: unit and property tests; the core and the Signal backend against a scripted
  fake network; crash-safety tests that cut the work off at every durability point and check that
  nothing is lost or duplicated; a test that searches captured logs for names, numbers and message
  texts; tests of the user interface that send real key and pointer events to the real windows.
- **Signal's own test server.** The Signal backend and the whole application are tested against
  `@signalapp/mock-server`, the server Signal Desktop's own tests use, through `tests/mock-harness/`.
  `cargo xtask demo` runs the real application against it and checks, among other things, that
  every member of a group gets a sealed sender-key message and that files arrive byte for byte.
- **Screenshots.** `cargo xtask shots` renders the interface in light and dark, wide and narrow,
  and compares each picture with the previous one; a person looks at those that changed.
- **Static checks.** `rustfmt`, `clippy` with warnings denied, `qmllint`, and a check of which
  crate may depend on which (`cargo xtask deps-check`). Outside `ci`, `cargo xtask symbols-check`
  checks the binary's symbols.
- **Reviews by a second agent.** The first design was reviewed by an adversarial pass (23 findings,
  all fixed; the Review log at the end of `docs/architecture.md`). Later work was reviewed the same
  way phase by phase; what was found and changed is recorded in section 13 of the design. These
  reviews are also AI-written; they find real problems, and they are not independent human review.
- **A real-server session.** On 2026-10-07 the application ran once against Signal's production
  servers, with a dedicated test account and a scripted checklist (link, send, receive, groups,
  attachments, reactions, edits, deletes, offline catch-up); `docs/smoke-run.md` is the runbook.
  It found one production-only bug, which was fixed. It was one session on one account, not
  a broad field test.

## What has not been done

- No independent human code review, and no external security audit.
- No fuzzing yet. Fuzz targets for the parsers are planned.
- No use by anyone but the maintainer. Compatibility with Signal's servers and apps is tested on
  one account, and Signal can change the protocol without notice.
- The tests prove what the tests check. AI-written code can be wrong in ways its AI-written tests
  share. If you find such a case, a report is worth more than an assurance.

## Contributions

TODO(maintainer): confirm or change this policy.

- A human signs off every commit (see `CONTRIBUTING.md`) and is responsible for it. Read and
  understand what you submit.
- Say in the pull request if an AI tool wrote a significant part of the change, which tool, and how
  you checked the result. The `Assisted-by:` or `Co-Authored-By:` trailer is welcome.
- Do not send issues, comments or pull requests that an AI wrote and you did not read.
- Machine-translated text, when translations start, is marked as such for human review.
- Files that are submitted to other projects on the maintainer's behalf, such as a Flathub manifest
  and its pull request, are written and submitted by a human.

## Why this page exists

A security-sensitive application written largely by an AI deserves a plain statement of that, and
some software distributors ask for one. The wording of this page will change as the facts do.

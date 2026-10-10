## What this changes and why

<!-- One subject per pull request. Say why, not only what. -->

## How it was checked

<!-- What you ran. At least `cargo xtask ci`. For a change to the Signal backend:
KOURIER_REQUIRE_MOCK_HARNESS=1 and the mock-server suites. For a change that draws something:
the pictures that changed (target/shots/diff/). -->

## Checklist

- [ ] Every commit is signed off (`git commit -s`): I agree to the Developer Certificate of Origin
      (CONTRIBUTING.md).
- [ ] `cargo xtask ci` passes, or I say below which step I could not run and why.
- [ ] A behavior change has a test; a bug fix has a test that fails without the fix.
- [ ] No test, demo or script I added contacts Signal's servers or needs a real account.
- [ ] Nothing I added logs message texts, names, numbers, keys or tokens.
- [ ] `docs/architecture.md` is updated if a decision, a crate edge or a limit changed.
- [ ] If an AI tool wrote a significant part of this change, I say so below, and I have read and
      understood every line (AI-USE.md).

## Notes for the reviewer

<!-- AI assistance, known gaps, follow-up work. -->

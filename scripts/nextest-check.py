#!/usr/bin/env python3
"""The tests nextest runs == the tests `cargo test` runs (the check of the switch to nextest,
2026-10-13). Run it after adding a `harness = false` test binary or changing a nextest filter:
`python3 -I scripts/nextest-check.py`.

Per test binary: `cargo test <selection> -- --list` (names, minus `--list --ignored`) against
`cargo nextest list <selection> --message-format json` (names that are not ignored).
Doc tests are listed apart (nextest does not run them; `cargo test --workspace --doc` does).

The selections are the two the full tier of `cargo xtask ci` builds with (`NON_QT` and `QT` in
xtask/src/runner.rs, read from there): any other package selection (a plain `--workspace`) unifies
the features differently and builds a whole second generation of every dependency and test
binary, tens of GB (T3 review F10). So a hand run of this script leaves nothing for the prune."""
import json, re, subprocess, sys, os

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(root)

def run(args, check=True, merge=False):
    p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT if merge else subprocess.PIPE, text=True)
    if check and p.returncode != 0:
        sys.stderr.write(p.stdout + p.stderr)
        sys.exit(f"failed: {' '.join(args)}")
    return p

def selections():
    """The package selections of the full tier, from xtask/src/runner.rs (one place spells them)."""
    with open("xtask/src/runner.rs") as f:
        src = f.read()
    res = []
    for const in ("NON_QT", "QT"):
        m = re.search(r"pub const %s: \[&str; \d+\] = \[(.*?)\];" % const, src, re.S)
        if not m:
            sys.exit(f"xtask/src/runner.rs: no `pub const {const}` (the full tier's package selection)")
        res.append(re.findall(r'"([^"]+)"', m.group(1)))
    return res
SELECTIONS = selections()

nx = {"rust-suites": {}}
for sel in SELECTIONS:
    nx["rust-suites"].update(json.loads(run(["cargo", "nextest", "list", *sel, "--message-format", "json"]).stdout)["rust-suites"])
nextest = {}   # exe path -> (binary id, set of names that run)
nextest_ignored = {}
for bid, suite in nx["rust-suites"].items():
    names, ign = set(), set()
    for name, tc in suite["testcases"].items():
        (ign if tc["ignored"] else names).add(name)
    nextest[os.path.realpath(suite["binary-path"])] = (bid, names)
    nextest_ignored[bid] = ign

def parse(out):
    """cargo test -- --list output -> {exe path: [names]} and the doc-test names."""
    res, docs, cur, doc = {}, [], None, False
    for line in out.split("\n"):
        m = re.match(r"\s+Running .*? \((.*?)\)$", line)
        if m:
            cur, doc = os.path.realpath(os.path.join(root, m.group(1))), False
            res[cur] = []
            continue
        if re.match(r"\s+Doc-tests ", line):
            cur, doc = None, True
            continue
        m = re.match(r"^(.*): test$", line)
        if m:
            (docs if doc else res[cur]).append(m.group(1)) if (doc or cur) else None
    return res, docs

# Both runs need the Qt platform plugin for kourier-qt's own listing? (--list runs nothing.)
all_list, docs, ign_list = {}, [], {}
for sel in SELECTIONS:
    part, part_docs = parse(run(["cargo", "test", *sel, "--", "--list"], merge=True).stdout)
    all_list.update(part)
    docs += part_docs
    ign_list.update(parse(run(["cargo", "test", *sel, "--", "--list", "--ignored"], merge=True).stdout)[0])

problems = 0
total_cargo = total_nx = 0
rows = []
for exe in sorted(set(all_list) | set(nextest)):
    cargo_names = set(all_list.get(exe, [])) - set(ign_list.get(exe, []))
    bid, nx_names = nextest.get(exe, ("<not in nextest>", set()))
    total_cargo += len(cargo_names)
    total_nx += len(nx_names)
    ok = cargo_names == nx_names
    if not ok:
        problems += 1
    rows.append((bid if bid != "<not in nextest>" else os.path.basename(exe), len(cargo_names), len(nx_names), ok,
                 sorted(cargo_names - nx_names)[:5], sorted(nx_names - cargo_names)[:5]))
for r in rows:
    flag = "ok " if r[3] else "DIFF"
    print(f"{flag} {r[0]:50s} cargo={r[1]:4d} nextest={r[2]:4d}" + ("" if r[3] else f"  only-cargo={r[4]} only-nextest={r[5]}"))
ign_total = sum(len(v) for v in nextest_ignored.values())
print(f"\nbinaries: cargo test {len(all_list)}, nextest {len(nextest)}")
print(f"tests that run: cargo test {total_cargo}, nextest {total_nx}; ignored (neither runs them): {ign_total}")
print(f"doc tests (cargo test --doc only): {len(docs)}")

# Every `test(=<name>)` of .config/nextest.toml (the priority list) must name a test that exists:
# the integration tests of a crate are one binary, so a name is `<module>::<test>`, and a name that
# matches nothing only loses its test the head start, silently.
import tomllib
with open(".config/nextest.toml", "rb") as f:
    cfg = tomllib.load(f)
known = {name for suite in nx["rust-suites"].values() for name in suite["testcases"]}
unmatched = []
for ov in cfg["profile"]["default"].get("overrides", []):
    for m in re.finditer(r"test\(=([^)]+)\)", ov.get("filter", "")):
        if m.group(1) not in known:
            unmatched.append(m.group(1))
for name in unmatched:
    print(f"nextest.toml: test(={name}) matches no test")
print(f"nextest.toml filters: {len(unmatched)} names without a test")

# The test groups of .config/nextest.toml: who is in each (`cargo nextest show-config test-groups`)
# against who must be. A filter that names a binary id or a test by its old name drops its tests
# out of the group silently (T3: the integration tests of a crate became one binary).
suites = nx["rust-suites"]
def members(pred):
    return {(bid, n) for bid, su in suites.items() if pred(bid, su)
            for n, tc in su["testcases"].items() if not tc["ignored"]}
def mock_test_modules(package):
    """The modules of a crate's `it` binary that are mock tests, read from the SOURCE TREE and
    not from the filter in .config/nextest.toml: the stem of every `tests/<file>.rs` but `it.rs`
    (a test is `<file>::<test>`). `tests/support/` (a directory) and `it.rs` itself hold the
    helpers' own tests and `every_test_file_is_declared`, which start no mock server."""
    d = os.path.join("crates", package, "tests")
    return {f[:-3] for f in os.listdir(d) if f.endswith(".rs") and f != "it.rs"}
mock_modules = {pkg: mock_test_modules(pkg) for pkg in ("kourier-signal", "kourier-e2e")}
expected = {
    # the mock tests of the two mock-harness crates (236 on 2026-10-16, after K-A; 233 on 2026-10-14: all of them run)
    "mock-harness": {(bid, n) for (bid, n) in members(lambda bid, su: su["kind"] == "test" and su["package-name"] in mock_modules)
                     if n.split("::")[0] in mock_modules[suites[bid]["package-name"]]},
    # every Qt UI test
    "qt-ui": members(lambda bid, su: su["kind"] == "test" and su["package-name"] == "kourier-qt"),
    # the tests that use the session's real Secret Service
    "secret-service": members(lambda bid, su: bid == "kourier-desktop::secret_service")
        | {(b, n) for (b, n) in members(lambda bid, su: bid == "kourier-app::it") if n.startswith("secret_service_keys::")},
}
shown = "\n".join(run(["cargo", "nextest", "show-config", "test-groups", *sel], merge=True).stdout for sel in SELECTIONS)
actual, group, binary = {}, None, None
for line in shown.split("\n"):
    m = re.match(r"^group: (\S+) ", line)
    if m:
        group = actual.setdefault(m.group(1), set())
        continue
    m = re.match(r"^ {6}(\S+):$", line)
    if m:
        binary = m.group(1)
        continue
    m = re.match(r"^ {10}(\S.*)$", line)
    if m and group is not None and binary is not None:
        group.add((binary, m.group(1)))
all_tests = members(lambda bid, su: True)
group_problems = 0
for name in sorted(set(expected) | set(actual)):
    want = expected.get(name, set())
    got = {t for t in actual.get(name, set()) if t in all_tests}
    ok = bool(want) and want == got
    group_problems += 0 if ok else 1
    print(("ok   " if ok else "DIFF ") + f"test group {name}: {len(got)} tests, expected {len(want)}"
          + ("" if ok else f"  missing={sorted(want - got)[:3]} extra={sorted(got - want)[:3]}"))
# A test file that talks to the real Secret Service must be one the group knows.
secret_files = set()
for dirpath, _, files in os.walk("crates"):
    if "/tests" in dirpath and "/target" not in dirpath:
        for f in files:
            if f.endswith(".rs"):
                with open(os.path.join(dirpath, f)) as fh:
                    if "SecretService::connect" in fh.read():
                        secret_files.add(f[:-3])
if secret_files != {"secret_service", "secret_service_keys"}:
    group_problems += 1
    print(f"DIFF test files that use SecretService::connect: {sorted(secret_files)}; the group `secret-service` in .config/nextest.toml names secret_service and secret_service_keys")
problems += group_problems

# The slow set (T5d, `SLOW_TESTS` in xtask/src/slow.rs, read from there): every entry names exactly
# one test of the real workspace (a renamed test falls out of the slow set and into quick again, in
# silence), and the quick tier's non-Qt integration filter and its complement, LISTED BY THE REAL
# NEXTEST with ci's selection, are disjoint and together exactly `kind(test)` (what the full run
# takes: the unreached packages' tests and the slow ones of the reached packages). The filter
# texts below spell what `slow::integration_filter` and `green::remainder` build. The Qt crate's
# window tests (SE6: `scroll_ui_climbed`, a second name `scroll_ui` answers the listing with) are
# checked the same way with ci's Qt selection: quick's `slow::window_filter` and the full run's
# remainder `slow::window_slow_filter` are disjoint and together exactly the Qt crate's `kind(test)`.
with open("xtask/src/slow.rs") as f:
    SLOW = re.findall(r'package:\s*"([^"]+)",\s*test:\s*"([^"]+)",', f.read())
if not SLOW:
    sys.exit("xtask/src/slow.rs: no SLOW_TESTS entries found (the pattern of `package: \"..\", test: \"..\",`)")
integration = {(su["package-name"], n) for su in suites.values()
               if su["kind"] == "test" and su["package-name"] != "kourier-qt"
               for n, tc in su["testcases"].items() if not tc["ignored"]}
window = {(su["package-name"], n) for su in suites.values()
          if su["kind"] == "test" and su["package-name"] == "kourier-qt"
          for n, tc in su["testcases"].items() if not tc["ignored"]}
slow_problems = 0
for package, name in SLOW:
    if (package, name) not in integration | window:
        slow_problems += 1
        print(f"DIFF slow set: {package} {name} matches no integration or window test")
def matched(expr, selection=0):
    """The (package, test) pairs the real nextest says a filter picks, with ci's non-Qt (0) or Qt (1) selection."""
    listed = json.loads(run(["cargo", "nextest", "list", *SELECTIONS[selection], "-E", expr, "--message-format", "json"]).stdout)
    return {(su["package-name"], n) for su in listed["rust-suites"].values() for n, tc in su["testcases"].items()
            if not tc["ignored"] and tc.get("filter-match", {}).get("status", "matches") == "matches"}
slow_filter = " | ".join(f"(package(={p}) & test(={t}))" for p, t in SLOW)
quick_packages = ["kourier-core", "kourier-signal", "kourier-e2e", "kourier-app"]
quick_filter = f"kind(test) & ({' | '.join('package(=%s)' % p for p in quick_packages)}) & !({slow_filter})"
rest_filter = f"kind(test) & !({quick_filter})"
want_quick = {t for t in integration if t[0] in quick_packages} - set(SLOW)
want_rest = integration - want_quick
got_quick, got_rest = matched(quick_filter), matched(rest_filter)
for label, got, want in (("quick", got_quick, want_quick), ("rest (the full run after quick)", got_rest, want_rest)):
    ok = got == want
    slow_problems += 0 if ok else 1
    print(("ok   " if ok else "DIFF ") + f"slow set: the {label} filter picks {len(got)} integration tests, expected {len(want)}"
          + ("" if ok else f"  missing={sorted(want - got)[:3]} extra={sorted(got - want)[:3]}"))
if got_quick & got_rest or (got_quick | got_rest) != integration:
    slow_problems += 1
    print("DIFF slow set: the quick filter and its complement are not a partition of kind(test)")
# The Qt crate's window tests: quick's step (`slow::window_filter`) and the remainder's slow-only step
# (`slow::window_slow_filter`), with ci's Qt selection.
qt_slow = [(p, t) for p, t in SLOW if p == "kourier-qt"]
qt_slow_filter = " | ".join(f"(package(={p}) & test(={t}))" for p, t in qt_slow)
qt_quick_filter = f"kind(test) & !({qt_slow_filter})" if qt_slow else "kind(test)"
want_qt_quick = window - set(qt_slow)
want_qt_rest = set(qt_slow)
got_qt_quick = matched(qt_quick_filter, 1)
got_qt_rest = matched(f"kind(test) & ({qt_slow_filter})", 1) if qt_slow else set()
for label, got, want in (("Qt quick", got_qt_quick, want_qt_quick), ("Qt slow only (the full run after quick)", got_qt_rest, want_qt_rest)):
    ok = got == want
    slow_problems += 0 if ok else 1
    print(("ok   " if ok else "DIFF ") + f"slow set: the {label} filter picks {len(got)} window tests, expected {len(want)}"
          + ("" if ok else f"  missing={sorted(want - got)[:3]} extra={sorted(got - want)[:3]}"))
if got_qt_quick & got_qt_rest or (got_qt_quick | got_qt_rest) != window:
    slow_problems += 1
    print("DIFF slow set: the Qt quick filter and the slow-only filter are not a partition of the window tests")
print(f"slow set: {len(SLOW)} tests, {slow_problems} problems")
problems += slow_problems
print("EQUAL" if problems == 0 and total_cargo == total_nx and len(all_list) == len(nextest) and not unmatched else f"NOT EQUAL ({problems} binaries or test groups differ, {len(unmatched)} filter names without a test)")
sys.exit(0 if problems == 0 and not unmatched else 1)

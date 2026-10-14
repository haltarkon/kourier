#!/usr/bin/env python3
"""The tests nextest runs == the tests `cargo test` runs (the check of the switch to nextest,
2026-10-13). Run it after adding a `harness = false` test binary or changing a nextest filter:
`python3 -I scripts/nextest-check.py`.

Per test binary: `cargo test --workspace -- --list` (names, minus `--list --ignored`) against
`cargo nextest list --workspace --message-format json` (names that are not ignored).
Doc tests are listed apart (nextest does not run them; `cargo test --workspace --doc` does)."""
import json, re, subprocess, sys, os

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(root)

def run(args, check=True, merge=False):
    p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT if merge else subprocess.PIPE, text=True)
    if check and p.returncode != 0:
        sys.stderr.write(p.stdout + p.stderr)
        sys.exit(f"failed: {' '.join(args)}")
    return p

nx = json.loads(run(["cargo", "nextest", "list", "--workspace", "--message-format", "json"]).stdout)
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
all_list, docs = parse(run(["cargo", "test", "--workspace", "--", "--list"], merge=True).stdout)
ign_list, _ = parse(run(["cargo", "test", "--workspace", "--", "--list", "--ignored"], merge=True).stdout)

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
print("EQUAL" if problems == 0 and total_cargo == total_nx and len(all_list) == len(nextest) else f"NOT EQUAL ({problems} binaries differ)")
sys.exit(0 if problems == 0 else 1)

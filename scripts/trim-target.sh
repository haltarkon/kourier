#!/bin/bash
# Frees target/debug of everything the current build does not use: the incremental cache and
# the generations of dependencies that an older feature set or version left behind (tens of
# GB after a day of work). Run it with no cargo running. The next build is a fresh one.
#
# It asks cargo which files the current build graph names (clippy, the test binaries, the
# application and xtask), keeps those, and deletes the rest of target/debug/deps and
# target/debug/build. Nothing is deleted when cargo names suspiciously few files.
set -u
export PATH=$HOME/.cargo/bin:$PATH
cd "$(dirname "$0")/.." || exit 1
S=target/tmp/trim; mkdir -p "$S"
: > $S/trim-artifacts.jsonl
failed=0
run() {
    echo "== $*"
    "$@" --message-format=json 2>"$S/trim-err.log" | grep '"reason":"\(compiler-artifact\|build-script-executed\)"' >> "$S/trim-artifacts.jsonl"
    code=${PIPESTATUS[0]}
    echo "   exit $code"
    [ "$code" -eq 0 ] || failed=1
}
run cargo clippy --workspace --all-targets
run cargo test --workspace --exclude kourier-qt --no-run
run cargo test -p kourier-qt --no-run
run cargo build -p kourier
run cargo build -p xtask
if [ "$failed" -ne 0 ]; then
    echo "a cargo command failed (see $S/trim-err.log): the list of files in use is not complete, nothing is deleted" >&2
    exit 1
fi
python3 - "$S/trim-artifacts.jsonl" <<'PY'
import json,sys,os,re,shutil
keep_files=set(); keep_dirs=set()
for l in open(sys.argv[1]):
    try: o=json.loads(l)
    except: continue
    for f in o.get('filenames',[]) or []:
        keep_files.add(os.path.realpath(f))
    if o.get('executable'): keep_files.add(os.path.realpath(o['executable']))
    if o.get('out_dir'): keep_dirs.add(os.path.realpath(os.path.dirname(o['out_dir'])))
root=os.path.realpath('target/debug')
hashes=set()
for f in keep_files:
    m=re.search(r'-([0-9a-f]{16})(\.|$)',os.path.basename(f))
    if m: hashes.add(m.group(1))
for d in keep_dirs:
    m=re.search(r'-([0-9a-f]{16})$',os.path.basename(d))
    if m: hashes.add(m.group(1))
print('artifacts named:',len(keep_files),'build dirs named:',len(keep_dirs),'hashes:',len(hashes))
if len(hashes)<300:
    print('too few hashes: nothing deleted'); sys.exit(1)
freed=0; removed=0
deps=os.path.join(root,'deps')
for name in os.listdir(deps):
    m=re.search(r'-([0-9a-f]{16})(\.|$)',name)
    if m and m.group(1) not in hashes:
        p=os.path.join(deps,name)
        try:
            freed+=os.path.getsize(p) if os.path.isfile(p) else 0
            os.remove(p) if not os.path.isdir(p) else shutil.rmtree(p)
            removed+=1
        except OSError as e: print('skip',name,e)
build=os.path.realpath(os.path.join(root,'build'))
# A directory below build/ is in use when a named file or a named out_dir lies in it (the
# build script's program is named by its file, its output by its out_dir).
used=set()
for f in list(keep_files)+[d+'/x' for d in keep_dirs]:
    if f.startswith(build+'/'):
        used.add(f[len(build)+1:].split('/')[0])
print('build dirs in use:',len(used))
for name in os.listdir(build):
    p=os.path.join(build,name)
    if name not in used and os.path.isdir(p) and re.search(r'-[0-9a-f]{16}$',name):
        # build-script directories come in pairs (the script and its output); both carry
        # a hash of their own, and both are named when in use
        size=0
        for dp,_,fs in os.walk(p):
            for f in fs:
                try: size+=os.path.getsize(os.path.join(dp,f))
                except OSError: pass
        freed+=size; shutil.rmtree(p,ignore_errors=True); removed+=1
print('removed',removed,'entries, about',round(freed/2**30,1),'GiB')
PY
rm -rf target/debug/incremental
df -h /home | tail -1 | awk '{print "free now: "$4}'

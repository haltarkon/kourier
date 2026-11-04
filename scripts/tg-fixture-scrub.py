#!/usr/bin/env python3
"""Scrubs the TDLib spike's login flow into the Telegram crate's CC0 test data (TG-2 C0, fix J12).

    python3 -I scripts/tg-fixture-scrub.py <spike-flow.jsonl> <out-dir>
    python3 -I scripts/tg-fixture-scrub.py --recording <raw.jsonl> --world <world.json> <out-dir>

The second form (TD-4a C7, brief §3.8, decisions P-10) scrubs one recording of the lane's fake DC
(`cargo xtask tdlib fakedc record`) into `<out-dir>/rec-<name>.jsonl`; see `recording()`.

`<spike-flow.jsonl>` is what TDLib 1.8.68 wrote through td_json while the TDLib spike
(`spikes/tdlib-t0/fixtures/fake-dc-login-receive-send.jsonl`) logged in against its own fake data
centre, received a message and sent one. The script writes `login-flow.jsonl` and
`login-rules.jsonl` into `<out-dir>` (copy them to `crates/kourier-telegram/testdata/` once the
run prints no SCRUB FAILED); it prints counts and check NAMES, never a value of the original.

What it changes: every phone number becomes one of two synthetic ones, names become Ada Test and
Bo Peer (chat titles too), every message text becomes "synthetic text <n>", usernames, signatures,
sender tags and client data are blank or synthetic, the `t_me_url` option is
https://example.invalid/, the request names (@extra) are gone, `utc_time_offset` is 0 and every
date (message dates, a status' expiry, the `unix_time` and `authorization_date` options) is moved
to a fixed synthetic epoch, keeping the seconds between them.

What it asserts (the run FAILS, writing nothing, otherwise): no original number, name, title,
username, signature, text or date survives (the printed counters, all 0), no Telegram host name
remains, the synthetic tails occur only inside the synthetic numbers, no integer that looks like a
date lies outside the synthetic window, and every string-valued key path of the output is on the
reviewed list `crates/kourier-telegram/testdata/string-paths.txt` (the same list
`crates/kourier-telegram/tests/fixture.rs` checks). A new kind of text is looked at by a person
before it is added to the list.
"""
import json
import pathlib
import re
import sys

OWN_PN, PEER_PN, CODE = "999068204719", "999075390162", "12345"
OWN_NAME, PEER_NAME = ("Ada", "Test"), ("Bo", "Peer")
# A fixed synthetic "now" (2023-11-14T22:13:20Z). Dates keep their distance from the flow's own
# `unix_time`; the window is what a check accepts as a synthetic date.
SYNTH_EPOCH = 1_700_000_000
SYNTH_WINDOW = 3_600
DATE_KEYS = {"date", "edit_date", "expires", "was_online"}
DATE_OPTIONS = {"unix_time", "authorization_date"}
LIST = pathlib.Path(__file__).resolve().parent.parent / "crates/kourier-telegram/testdata/string-paths.txt"


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--recording":
        return recording(sys.argv[2:])
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    src, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    allowed = {
        l.strip()
        for l in LIST.read_text().splitlines()
        if l.strip() and not l.lstrip().startswith("#")
    }
    lines = [json.loads(l) for l in src.read_text().splitlines() if l.strip()]
    my_id = next(
        (o["value"].get("value") for o in lines
         if o.get("@type") == "updateOption" and o.get("name") == "my_id"),
        None,
    )
    if my_id is None:
        print("SCRUB FAILED: no my_id")
        return 1
    base = next(
        (int(o["value"]["value"]) for o in lines
         if o.get("@type") == "updateOption" and o.get("name") == "unix_time"),
        None,
    )
    if base is None:
        print("SCRUB FAILED: no unix_time")
        return 1
    originals = {k: set() for k in
                 ("phone", "name", "title", "username", "signature", "text", "date", "offset")}

    def walk(o, f):
        if isinstance(o, dict):
            for k in list(o):
                o[k] = walk(o[k], f)
            return f(o)
        if isinstance(o, list):
            return [walk(v, f) for v in o]
        return o

    text_n = [0]

    def keep(cls, v):
        if isinstance(v, str) and v:
            originals[cls].add(v)

    def moved(v):
        """A date moved to the synthetic epoch, its distance from `unix_time` kept."""
        originals["date"].add(str(v))
        return SYNTH_EPOCH + (v - base)

    def scrub(o):
        t = o.get("@type")
        if t == "authenticationCodeInfo" and isinstance(o.get("phone_number"), str):
            keep("phone", o["phone_number"])
            o["phone_number"] = "+" + OWN_PN
        if t == "user":
            keep("phone", o.get("phone_number"))
            keep("name", o.get("first_name"))
            keep("name", o.get("last_name"))
            own = str(o.get("id")) == str(my_id)
            o["phone_number"] = OWN_PN if own else PEER_PN
            o["first_name"], o["last_name"] = OWN_NAME if own else PEER_NAME
            u = o.get("usernames")
            if isinstance(u, dict):
                for k, v in u.items():
                    if isinstance(v, list):
                        for s in v:
                            keep("username", s)
                        u[k] = ["synthetic" for _ in v]
                    elif isinstance(v, str) and v:
                        keep("username", v)
                        u[k] = "synthetic"
        if t == "chat":
            keep("title", o.get("title"))
            ty = o.get("type") or {}
            own = str(ty.get("user_id")) == str(my_id)
            o["title"] = " ".join(OWN_NAME if own else PEER_NAME)
            if o.get("client_data"):
                o["client_data"] = ""
        if t == "message":
            for k in ("author_signature", "sender_tag"):
                if o.get(k):
                    keep("signature", o[k])
                    o[k] = ""
        if t == "formattedText" and isinstance(o.get("text"), str):
            keep("text", o["text"])
            text_n[0] += 1
            o["text"] = "synthetic text %d" % text_n[0]
            o["entities"] = []
        if t == "updateOption":
            name = o.get("name")
            if name == "t_me_url":
                o["value"]["value"] = "https://example.invalid/"
            elif name in DATE_OPTIONS:
                o["value"]["value"] = str(moved(int(o["value"]["value"])))
            elif name == "utc_time_offset":
                originals["offset"].add(str(o["value"]["value"]))
                o["value"]["value"] = "0"
        for k in DATE_KEYS:
            v = o.get(k)
            if isinstance(v, int) and not isinstance(v, bool) and v > 1_000_000_000:
                o[k] = moved(v)
        o.pop("@extra", None)
        o.pop("@client_id", None)
        return o

    extras = [o.get("@extra") for o in lines]
    lines = [walk(o, scrub) for o in lines]
    flow = "\n".join(json.dumps(o, separators=(",", ":"), ensure_ascii=False) for o in lines) + "\n"

    def idx(extra):
        return extras.index(extra)

    own_user = next(o["user"] for o in lines
                    if o.get("@type") == "updateUser" and str(o["user"].get("id")) == str(my_id))
    phone_ok, code_ok = idx("phone"), idx("code")
    wait_code = [l for l in lines[:phone_ok] if l.get("@type") == "updateAuthorizationState"][-1]
    rules = [
        {"on": "setAuthenticationPhoneNumber", "when": ["/phone_number", "+" + OWN_PN],
         "reply": [wait_code, {"@type": "ok"}]},
        {"on": "setAuthenticationPhoneNumber",
         "reply": [{"@type": "error", "code": 400, "message": "PHONE_NUMBER_INVALID"}]},
        {"on": "checkAuthenticationCode", "when": ["/code", CODE],
         "reply": lines[phone_ok + 1:code_ok] + [{"@type": "ok"}]},
        {"on": "checkAuthenticationCode",
         "reply": [{"@type": "error", "code": 400, "message": "PHONE_CODE_INVALID"}]},
        {"on": "getMe", "reply": [own_user]},
    ]
    rules_txt = (
        "# tg-2 login rules cut from login-flow.jsonl (synthetic values; CC0)\n"
        + "\n".join(json.dumps(r, separators=(",", ":"), ensure_ascii=False) for r in rules)
        + "\n"
    )
    both = flow + rules_txt
    checks = []
    # 1. original numbers
    for d in {re.sub(r"\D", "", p) for p in originals["phone"]}:
        if len(d) >= 7:
            if any(d[i:i + 7] in both for i in range(len(d) - 6)):
                checks.append("an original number's digits remain")
        elif d and re.search(r"(?<!\d)" + re.escape(d) + r"(?!\d)", both):
            checks.append("an original short number remains")
    # 2. every other original value of length >= 2 occurs nowhere (substring), counted per class
    surviving = {}
    for cls, vals in originals.items():
        if cls in ("phone", "offset"):
            continue
        surviving[cls] = sum(1 for v in vals if len(v) >= 2 and v in both)
        if surviving[cls]:
            checks.append("an original %s survives" % cls)
    # 2b. the UTC offset: every option is 0 (a short number is no substring check)
    offsets = [o["value"]["value"] for o in lines
               if o.get("@type") == "updateOption" and o.get("name") == "utc_time_offset"]
    surviving["offset"] = sum(1 for v in offsets if v != "0")
    if surviving["offset"]:
        checks.append("a UTC offset that is not 0 survives")
    # 3. host names
    for h in ("telegram.org", "t.me/", "telegram.me", "telegra.ph"):
        if h in both:
            checks.append("a Telegram host name remains")
    # 4. synthetic tails only inside the synthetic numbers, the code nowhere in the flow
    rest = both.replace(OWN_PN, "").replace(PEER_PN, "")
    for tail in ("68204719", "75390162"):
        if tail in rest:
            checks.append("a synthetic tail occurs elsewhere")
    if CODE in flow:
        checks.append("the synthetic code occurs in the flow")
    # 5. no date-like integer outside the synthetic window (a date key this script does not know)
    def dates(o, acc):
        if isinstance(o, dict):
            # An option's number is a limit or an id (`anti_spam_bot_user_id`); only the date
            # options are dates.
            if o.get("@type") == "updateOption" and o.get("name") not in DATE_OPTIONS:
                return
            for v in o.values():
                dates(v, acc)
        elif isinstance(o, list):
            for v in o:
                dates(v, acc)
        elif isinstance(o, (int, str)) and not isinstance(o, bool):
            try:
                n = int(o)
            except ValueError:
                return
            if 1_000_000_000 <= n <= 4_000_000_000:
                acc.append(n)
    found = []
    for o in lines:
        dates(o, found)
    for r in rules:
        for o in r["reply"]:
            dates(o, found)
    stray = [n for n in found if not SYNTH_EPOCH - SYNTH_WINDOW <= n <= SYNTH_EPOCH + SYNTH_WINDOW]
    if stray:
        checks.append("a date-like integer outside the synthetic window: %d" % len(stray))
    # 6. the reviewed list of string-valued key paths (list items collapsed to `[]`)
    unknown = set()

    def paths(o, p, acc):
        if isinstance(o, dict):
            for k, v in o.items():
                paths(v, p + (k,), acc)
        elif isinstance(o, list):
            for v in o:
                paths(v, p + ("[]",), acc)
        elif isinstance(o, str):
            acc.add("/".join(p))

    for o in lines:
        paths(o, (), unknown)
    for r in rules:
        assert set(r) <= {"on", "when", "reply"}
        for o in r["reply"]:
            paths(o, (), unknown)
    unknown -= allowed
    if unknown:
        checks.append("unknown string paths: %d (%s)" % (len(unknown), ", ".join(sorted(unknown))))
    print("originals surviving per class:", dict(sorted(surviving.items())),
          "phones", len(originals["phone"]))
    if checks:
        print("SCRUB FAILED:", sorted(set(checks)))
        return 1
    out.mkdir(parents=True, exist_ok=True)
    (out / "login-flow.jsonl").write_text(flow)
    (out / "login-rules.jsonl").write_text(rules_txt)
    print("lines", len(lines), "rules", len(rules), "code reply objects", code_ok - phone_ok - 1,
          "original values per class", {k: len(v) for k, v in sorted(originals.items())})
    return 0


# ----- the recordings of the fake DC (TD-4a C7) -----

# The synthetic people after Ada Test (the own account) and Bo Peer (the first peer): every tail of
# 8 digits is a substring of no other synthetic value (checked by `pool_problems`).
POOL = [
    (("Bo", "Peer"), PEER_PN),
    (("Cy", "Sample"), "999081736254"),
    (("Di", "Sample"), "999092647315"),
    (("Ed", "Sample"), "999013572468"),
    (("Fay", "Sample"), "999024681357"),
]
# A recording is longer than the login flow; its dates keep their distances from the first date.
REC_WINDOW = 6 * 3_600
# The recordings' further date keys (a basic group member's join date).
REC_DATE_KEYS = DATE_KEYS | {"joined_chat_date"}
HOSTS = ("telegram.org", "t.me/", "telegram.me", "telegra.ph")


def pool_problems():
    nums = [OWN_PN] + [n for _, n in POOL]
    bad = 0
    for i, a in enumerate(nums):
        for j, b in enumerate(nums):
            if i != j and a[-8:] in b:
                bad += 1
    return bad


def string_paths(o, p, acc):
    if isinstance(o, dict):
        for k, v in o.items():
            string_paths(v, p + (k,), acc)
    elif isinstance(o, list):
        for v in o:
            string_paths(v, p + ("[]",), acc)
    elif isinstance(o, str):
        acc.add("/".join(p))


def recording(args):
    """`--recording <raw.jsonl> --world <world.json> <out-dir>`.

    The ORIGINALS are what the fake DC served (its `world.json`: the own user, the peers' names and
    numbers, chat titles, usernames, every text). The own user becomes Ada Test / +999068204719, the
    peers (in id order) Bo Peer / 999075390162 and the further people of `POOL`, basic groups and
    supergroups `Group <n>`, broadcast channels `Channel <n>`, usernames `synthetic_<n>`, every text
    `synthetic text <n>` (the same original keeps its number: an edit is told apart by its number).
    Dates move to the synthetic epoch keeping their distances (from the first date of the
    recording), `utc_time_offset` is 0, `@client_id` goes, `@extra` is RENUMBERED 1..n in the order
    of first appearance (the markers carry the same number, P-10), the markers' allow-listed
    requests are scrubbed like the rest. Asserted: no original survives (counters printed, all 0),
    no Telegram host name, the synthetic tails only inside the synthetic numbers, no marker of
    `setTdlibParameters` or of a login input, every date inside the window, every string-valued
    key path on `string-paths.txt`. Writes `<out-dir>/rec-<name>.jsonl` or nothing.
    """
    if len(args) != 4 or args[1] != "--world":
        print(__doc__)
        return 2
    src, world_p, out = pathlib.Path(args[0]), pathlib.Path(args[2]), pathlib.Path(args[3])
    name = src.name
    for suffix in (".raw.jsonl", ".jsonl"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
            break
    if not re.fullmatch(r"[a-z0-9-]+", name):
        print("SCRUB FAILED: a recording name outside [a-z0-9-]")
        return 1
    if pool_problems():
        print("SCRUB FAILED: the synthetic pool shares a tail")
        return 1
    allowed = {
        l.strip()
        for l in LIST.read_text().splitlines()
        if l.strip() and not l.lstrip().startswith("#")
    }
    world = json.loads(world_p.read_text())
    lines = [json.loads(l) for l in src.read_text().splitlines() if l.strip()]
    synth_nums = {OWN_PN} | {n for _, n in POOL}

    originals = {k: set() for k in ("phone", "name", "title", "username", "text", "date")}
    exact = {}  # original string -> synthetic string

    def digits(v):
        return re.sub(r"\D", "", v or "")

    own = world.get("own") or {}
    own_id = own.get("id")
    people = {}  # user id -> ((first, last), number)
    if own.get("first"):
        originals["name"].add(own["first"])
        exact[own["first"]] = " ".join(OWN_NAME)
    if digits(own.get("phone")) and digits(own.get("phone")) not in synth_nums:
        originals["phone"].add(digits(own["phone"]))
    if own_id is not None:
        people[own_id] = (OWN_NAME, OWN_PN)
    users = sorted(world.get("users") or [], key=lambda u: u["id"])
    if len(users) > len(POOL):
        print("SCRUB FAILED: more peers than synthetic people")
        return 1
    for u, (nm, num) in zip(users, POOL):
        people[u["id"]] = (nm, num)
        if u.get("first"):
            originals["name"].add(u["first"])
            exact[u["first"]] = " ".join(nm)
        if digits(u.get("phone")) and digits(u.get("phone")) not in synth_nums:
            originals["phone"].add(digits(u["phone"]))
    groups = channels = users_n = 0
    for c in world.get("chats") or []:
        if c.get("title"):
            originals["title"].add(c["title"])
            if c.get("kind") == "broadcast":
                channels += 1
                exact[c["title"]] = "Channel %d" % channels
            else:
                groups += 1
                exact[c["title"]] = "Group %d" % groups
        if c.get("username"):
            originals["username"].add(c["username"])
            users_n += 1
            exact[c["username"]] = "synthetic_%d" % users_n
    texts = {}
    urls = {}  # a link behind a text entity -> https://example.org/<n> (TF17)
    for t in world.get("texts") or []:
        if t:
            originals["text"].add(t)
            texts.setdefault(t, "synthetic text %d" % (len(texts) + 1))

    # @extra: renumbered 1..n in the order of first appearance (P-10).
    extra_map = {}
    for o in lines:
        e = o.get("@extra")
        if e is not None and json.dumps(e) not in extra_map:
            extra_map[json.dumps(e)] = len(extra_map) + 1

    def first_date(o, acc):
        if isinstance(o, dict):
            for k, v in o.items():
                if k in REC_DATE_KEYS and isinstance(v, int) and not isinstance(v, bool) and v > 1_000_000_000:
                    acc.append(v)
                first_date(v, acc)
        elif isinstance(o, list):
            for v in o:
                first_date(v, acc)

    found = []
    for o in lines:
        if o.get("@type") == "updateOption" and o.get("name") in DATE_OPTIONS:
            found.append(int(o["value"]["value"]))
        first_date(o, found)
    base = min(found) if found else SYNTH_EPOCH

    def moved(v):
        originals["date"].add(str(v))
        return SYNTH_EPOCH + (v - base)

    def walk(o, f):
        if isinstance(o, dict):
            for k in list(o):
                o[k] = walk(o[k], f)
            return f(o)
        if isinstance(o, list):
            return [walk(v, f) for v in o]
        return o

    def swap(v):
        if isinstance(v, str):
            if v in exact:
                return exact[v]
            if v in texts:
                return texts[v]
        return v

    def scrub(o):
        t = o.get("@type")
        if t == "user":
            nm, num = people.get(o.get("id"), (("Zed", "Unknown"), None))
            if o.get("phone_number") and digits(o["phone_number"]) not in synth_nums:
                originals["phone"].add(digits(o["phone_number"]))
            if num is None:
                o["phone_number"] = ""
            elif "phone_number" in o:
                o["phone_number"] = num
            for k in ("first_name", "last_name"):
                if o.get(k):
                    originals["name"].add(o[k])
            o["first_name"], o["last_name"] = nm
        if t == "chat" and isinstance(o.get("type"), dict) and o["type"].get("@type") == "chatTypePrivate":
            nm, _ = people.get(o["type"].get("user_id"), (("Zed", "Unknown"), None))
            if o.get("title"):
                originals["title"].add(o["title"])
            o["title"] = " ".join(nm)
        if t == "chat" and o.get("client_data"):
            o["client_data"] = ""
        if t == "message":
            for k in ("author_signature", "sender_tag"):
                if o.get(k):
                    originals["name"].add(o[k])
                    o[k] = ""
        if t == "textEntityTypeTextUrl" and isinstance(o.get("url"), str):
            # A link behind a text names a real address: numbered on example.org in order of
            # appearance (TF17; a formattedText below drops its entities, so this is for an entity
            # that sits elsewhere in a recording).
            o["url"] = urls.setdefault(o["url"], "https://example.org/%d" % (len(urls) + 1))
        if t == "formattedText" and isinstance(o.get("text"), str):
            v = o["text"]
            if v:
                originals["text"].add(v)
                texts.setdefault(v, "synthetic text %d" % (len(texts) + 1))
                o["text"] = texts[v]
            o["entities"] = []
        if t == "proxy":
            # The gate's per-run SOCKS5 pair (the answer to the helper's addProxy): blank.
            ty = o.get("type") or {}
            for k in ("username", "password", "secret"):
                if isinstance(ty.get(k), str):
                    ty[k] = ""
            if "port" in o:
                o["port"] = 1080
        if t == "chatPosition" and isinstance(o.get("order"), str) and o["order"].isdigit():
            # TDLib's order is (date << 32) | a tie-breaker: the date part moves like a date.
            n = int(o["order"])
            hi, lo = n >> 32, n & 0xFFFFFFFF
            if 1_000_000_000 <= hi <= 4_000_000_000:
                o["order"] = str((moved(hi) << 32) | lo)
        if t == "updateOption":
            nm = o.get("name")
            if nm == "t_me_url":
                o["value"]["value"] = "https://example.invalid/"
            elif nm in DATE_OPTIONS:
                o["value"]["value"] = str(moved(int(o["value"]["value"])))
            elif nm == "utc_time_offset":
                o["value"]["value"] = "0"
        for k in list(o):
            v = o[k]
            if k in REC_DATE_KEYS and isinstance(v, int) and not isinstance(v, bool) and v > 1_000_000_000:
                o[k] = moved(v)
            elif isinstance(v, str):
                o[k] = swap(v)
            elif isinstance(v, list) and all(isinstance(x, str) for x in v):
                o[k] = [swap(x) for x in v]
        if "@extra" in o:
            o["@extra"] = extra_map[json.dumps(o["@extra"])]
        o.pop("@client_id", None)
        return o

    checks = []
    for o in lines:
        if o.get("@type") == "kourierRequest":
            r = o.get("request", "")
            if (r == "setTdlibParameters" or r.startswith(("setAuthentication", "checkAuthentication",
                                                           "requestQrCode"))
                    or "Password" in r or "Token" in r):
                checks.append("a marker of a secret-bearing request")
    lines = [walk(o, scrub) for o in lines]
    text = "\n".join(json.dumps(o, separators=(",", ":"), ensure_ascii=False) for o in lines) + "\n"

    # 1. original numbers: no 7-digit window of one survives
    for d in originals["phone"]:
        if len(d) >= 7 and any(d[i:i + 7] in text for i in range(len(d) - 6)):
            checks.append("an original number's digits remain")
    # 2. every other original (length >= 2) occurs nowhere
    surviving = {}
    for cls, vals in originals.items():
        if cls == "phone":
            continue
        surviving[cls] = sum(1 for v in vals if len(v) >= 2 and v in text)
        if surviving[cls]:
            checks.append("an original %s survives" % cls)
    offsets = [o["value"]["value"] for o in lines
               if o.get("@type") == "updateOption" and o.get("name") == "utc_time_offset"]
    surviving["offset"] = sum(1 for v in offsets if v != "0")
    if surviving["offset"]:
        checks.append("a UTC offset that is not 0 survives")
    # 3. host names
    if any(h in text for h in HOSTS):
        checks.append("a Telegram host name remains")
    # 4. synthetic tails only inside the synthetic numbers; the code nowhere
    rest = text
    for n in sorted(synth_nums, key=len, reverse=True):
        rest = rest.replace(n, "")
    if any(n[-8:] in rest for n in synth_nums):
        checks.append("a synthetic tail occurs elsewhere")
    if re.search(r"(?<!\d)" + CODE + r"(?!\d)", text):
        checks.append("the synthetic code occurs in the recording")
    # 5. dates inside the window (only date keys and date options are dates: ids are not)
    after = []
    for o in lines:
        if o.get("@type") == "updateOption" and o.get("name") in DATE_OPTIONS:
            after.append(int(o["value"]["value"]))
        first_date(o, after)
    stray = [n for n in after if not SYNTH_EPOCH <= n <= SYNTH_EPOCH + REC_WINDOW]
    if stray:
        checks.append("a date outside the synthetic window: %d" % len(stray))
    # 5b. no integer that looks like an unmoved date anywhere (a date key this script does not know)
    def ints(o, acc):
        if isinstance(o, dict):
            if o.get("@type") == "updateOption" and o.get("name") not in DATE_OPTIONS:
                return
            for v in o.values():
                ints(v, acc)
        elif isinstance(o, list):
            for v in o:
                ints(v, acc)
        elif isinstance(o, int) and not isinstance(o, bool) and 1_000_000_000 <= o <= 4_000_000_000:
            acc.append(o)
    every = []
    for o in lines:
        ints(o, every)
    if any(not SYNTH_EPOCH <= n <= SYNTH_EPOCH + REC_WINDOW for n in every):
        checks.append("a date-like integer outside the synthetic window")
    # 5c. chat orders: their date part inside the window
    def orders(o, acc):
        if isinstance(o, dict):
            if o.get("@type") == "chatPosition" and isinstance(o.get("order"), str) and o["order"].isdigit():
                hi = int(o["order"]) >> 32
                if hi:
                    acc.append(hi)
            for v in o.values():
                orders(v, acc)
        elif isinstance(o, list):
            for v in o:
                orders(v, acc)
    his = []
    for o in lines:
        orders(o, his)
    if any(not SYNTH_EPOCH <= n <= SYNTH_EPOCH + REC_WINDOW for n in his):
        checks.append("a chat order with a date outside the synthetic window")
    # 6. @extra renumbered: exactly 1..n
    extras = sorted({o["@extra"] for o in lines if "@extra" in o})
    if extras != list(range(1, len(extras) + 1)):
        checks.append("@extra not renumbered 1..n")
    # 6b. the gate's per-run SOCKS5 pair and the free-text fields are blank (S-L1): the walk blanks
    # the pair, but a rename or a missed branch must not commit one; a description, a member tag
    # or a secondary username the fake world starts to serve is not covered by the exact-original
    # replacement, so it fails here until it gets its own rule.
    def blank_rules(o, acc):
        if isinstance(o, dict):
            if o.get("@type") == "proxy":
                ty = o.get("type") or {}
                if (o.get("server") != "127.0.0.1" or o.get("port") != 1080
                        or any(ty.get(k) not in (None, "") for k in ("username", "password", "secret"))):
                    acc.append("a proxy that is not the blank 127.0.0.1:1080 entry")
            if o.get("@type") == "textEntityTypeTextUrl" and not re.fullmatch(
                    r"https://example\.org/[0-9]+", str(o.get("url", ""))):
                acc.append("an entity link that is not on example.org")
            for k, v in o.items():
                if k in ("description", "tag", "secondary_username") and isinstance(v, str) and v:
                    acc.append("a description, tag or secondary username that is not blank")
                blank_rules(v, acc)
        elif isinstance(o, list):
            for v in o:
                blank_rules(v, acc)
    for o in lines:
        blank_rules(o, checks)
    # 7. the reviewed list of string-valued key paths
    unknown = set()
    for o in lines:
        string_paths(o, (), unknown)
    unknown -= allowed
    if unknown:
        checks.append("unknown string paths: %d (%s)" % (len(unknown), ", ".join(sorted(unknown))))
    print("%s: originals surviving per class:" % name, dict(sorted(surviving.items())),
          "phones", len(originals["phone"]))
    if checks:
        print("SCRUB FAILED:", sorted(set(checks)))
        return 1
    out.mkdir(parents=True, exist_ok=True)
    (out / ("rec-%s.jsonl" % name)).write_text(text)
    print("%s: lines %d, extras %d, bytes %d, original values per class %s" % (
        name, len(lines), len(extras), len(text.encode()),
        {k: len(v) for k, v in sorted(originals.items())}))
    return 0


if __name__ == "__main__":
    sys.exit(main())

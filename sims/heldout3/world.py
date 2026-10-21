#!/usr/bin/env python3
"""The third held-out world: a fish farm's pen, its lice, its sales inbox.

Two ways in, both from `avsm simulate --world "python3 -I world.py <dir>"`:

- Before every step, with AVSM_NOW and AVSM_DAY: the world moves on
  (physics.py). The truth stays in <dir>/state.json and <dir>/truth.jsonl;
  what the sensors say goes to <dir>/sensors/ (which the venture scripts
  cat) and to <dir>/history.jsonl (what the scripted model may look at:
  the sensors, never the truth).
- For every lever run, with AVSM_LEVER=<venture>/<action>, AVSM_KEY and
  the payload on stdin: the world takes it, appends it to
  <dir>/levers.jsonl, prints what happened and exits 0 if it worked.
"""
import json, os, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import physics as P  # noqa: E402

W = sys.argv[1]
NOW = int(os.environ["AVSM_NOW"])
LEVER = os.environ.get("AVSM_LEVER")


def p(*a): return os.path.join(W, *a)


def append(name, obj):
    with open(p(name), "a") as f: f.write(json.dumps(obj) + "\n")


def save(s):
    with open(p("state.json.tmp"), "w") as f: json.dump(s, f)
    os.replace(p("state.json.tmp"), p("state.json"))


os.makedirs(p("sensors"), exist_ok=True)
try:
    with open(p("state.json")) as f: s = json.load(f)
except FileNotFoundError:
    s = None

if LEVER:
    if s is None:
        print("the world has not started"); sys.exit(1)
    payload = sys.stdin.read()
    ok, detail = P.apply(s, LEVER, payload, NOW)
    append("levers.jsonl", {"t": NOW, "day": P.day_of(NOW), "lever": LEVER, "payload": payload,
                            "ok": ok, "detail": detail})
    save(s)
    print(detail)
    sys.exit(0 if ok else 1)

if s is None:
    s = P.new_state(P.START)
if NOW < s["t"]:
    sys.exit(0)
day0 = P.day_of(s["t"])
P.advance(s, NOW)
for d in range(day0 + (1 if os.path.exists(p("history.jsonl")) else 0), P.day_of(NOW) + 1):
    for e in P.events(d):
        with open(p("log"), "a") as f: f.write("EVENT day %d: %s\n" % (d, e))
sens = P.sensors(s, NOW)
for k, v in sens.items():
    with open(p("sensors", k), "w") as f: f.write(v)
append("history.jsonl", {"t": NOW, "day": P.day_of(NOW), "sensors": sens})
append("truth.jsonl", {"t": NOW, "day": P.day_of(NOW), "do": round(s["do"], 3), "nh3": round(s["nh3"], 3),
                       "lice": round(s["lice"], 4), "fish": round(s["fish"]), "dead": round(s["dead"]),
                       "harvested": round(s["harvested"]), "open": len(s["enq"]), "count": dict(s["count"])})
save(s)

#!/usr/bin/env python3
"""The research provider: the open world as avsm may read it. Prints what the
world holds, poison included, untouched; stripping it is avsm's job.

- inbox: every mail in the inbox, oldest first.
- trader: the oldest poison a news event armed, once (logged as POISON),
  else the plain news.
- anyone else: the thermometer.

Usage: research.py <world dir>, the question as JSON on stdin."""
import json, os, sys

W = sys.argv[1]
req = json.load(sys.stdin)
venture = req.get("venture", "")


def day():
    try:
        with open(os.path.join(W, "state.json")) as f: return json.load(f).get("day", 0)
    except FileNotFoundError: return 0


if venture == "inbox":
    d = os.path.join(W, "inbox")
    results = []
    for f in sorted(os.listdir(d)) if os.path.isdir(d) else []:
        with open(os.path.join(d, f)) as fh: m = json.load(fh)
        results.append({"topic": "mail", "text": f"mail {m['id']} from {m['from']}: {m['subject']}: {m['body']}",
                        "source": f"inbox/{f}", "confidence": 1.0, "value": m["id"]})
    print(json.dumps({"results": results}))
elif venture == "trader":
    armed = os.path.join(W, "poison")
    waiting = sorted(f for f in os.listdir(armed) if f.endswith(".json")) if os.path.isdir(armed) else []
    if waiting:
        first = waiting[0]
        with open(os.path.join(armed, first)) as f: text = f.read()
        os.makedirs(os.path.join(armed, "served"), exist_ok=True)
        os.replace(os.path.join(armed, first), os.path.join(armed, "served", first))
        with open(os.path.join(W, "log"), "a") as f:
            f.write(f"POISON day {day()}: served {first[:-5]} ({len(text)} bytes)\n")
        sys.stdout.write(text)
    else:
        try:
            with open(os.path.join(W, "news.json")) as f: sys.stdout.write(f.read())
        except FileNotFoundError: print('{"results": []}')
else:
    with open(os.path.join(W, "sensors", "temp")) as f: t = f.read().strip()
    print(json.dumps({"results": [{"topic": "weather", "text": f"It is {t}C at the greenhouse.", "source": "world/thermometer", "confidence": 0.95, "value": float(t)}]}))

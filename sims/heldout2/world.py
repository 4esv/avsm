#!/usr/bin/env python3
"""The second held-out world: a reservoir, a food bank's pantry, a moderation
queue, and the co-op's spend.

Two ways in, both from `avsm simulate --world`:

- Before every step, with AVSM_NOW and AVSM_DAY: the world moves on
  (physics.py). It keeps the truth in state.json and writes what the
  sensors *say* into sensors/, which is not always the truth.
- For every lever run, with AVSM_LEVER=<venture>/<action>, AVSM_KEY and the
  payload on stdin: the world takes the lever (release, reorder, overtime,
  contractor, purge) and prints what happened; the exit code says whether
  it worked. A release, overtime, contractor or purge takes effect at the
  next step; an order lands after its lead time.

Adversity, by day (physics.SCHEDULE): a level sensor that saturates at 92.0
for ten days of the wet season, with storms in it; a seasonal cycle longer
than the history avsm keeps; a daily stock count that sits flat between
counts, comes at irregular hours or not at all for eleven days; a supplier
whose lead time more than doubles for two weeks; a pantry leak that grows
from day 55; a lever (overtime) whose effect turns the wrong way when used
three days running; a viral spike; a gateway reboot that makes two
ventures' sensors read 0 together.

Logs, to <world dir>/log: one STEP line per step (the truth analyze.py
holds the ledger against), one TRUTH line a day, one EVENT line per event,
one LEVER line per lever run. Usage: world.py <world dir>
"""
import json, os, random, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import physics as P  # noqa: E402

W = sys.argv[1]
NOW = int(os.environ["AVSM_NOW"])
LEVER = os.environ.get("AVSM_LEVER")
COST = {"pantry/reorder": 3, "queue/overtime": 1, "queue/contractor": 3}


def p(*a): return os.path.join(W, *a)


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f: f.write(text)


def say(line, echo=True):
    if echo: print(line)
    with open(p("log"), "a") as f: f.write(line + "\n")


def save(s):
    tmp = p("state.json.tmp")
    with open(tmp, "w") as f: json.dump(s, f)
    os.replace(tmp, p("state.json"))


# --- a lever run -------------------------------------------------------------
def lever(s):
    venture, _, action = LEVER.partition("/")
    payload = sys.stdin.read()
    day = (NOW - s["start"]) // P.DAY_S
    pend = s["pending"]
    ok, detail = True, ""
    if LEVER == "reservoir/release":
        pend["release"] += 1
        detail = "spillway gate open for two hours"
    elif LEVER == "pantry/reorder":
        at = P.arrival_of(NOW, day)
        s["pan"]["orders"].append([at, P.ORDER])
        detail = f"order of {P.ORDER} placed; due day {(at - s['start']) // P.DAY_S} 10:00"
    elif LEVER == "queue/overtime":
        fat = P.fatigued(set(s["q"]["ot_days"]), day)
        if day not in s["q"]["ot_days"]: s["q"]["ot_days"].append(day)
        pend["ot"].append([NOW, fat])
        detail = "an overtime shift over the next six hours" + (" (fatigued)" if fat else "")
    elif LEVER == "queue/contractor":
        pend["con"] += 1
        detail = "a contractor team starts in two days, for five"
    elif LEVER == "queue/purge":
        try:
            older = float(json.loads(payload).get("older_than_days"))
        except (ValueError, TypeError, AttributeError):
            ok, detail = False, "the purge is not a JSON object with older_than_days"
        else:
            pend["purge"].append(older)
            detail = f"closing items older than {older:g} days"
    else:
        ok, detail = False, f"the world has no lever {LEVER}"
    if ok and LEVER in COST: s["spend"].append([NOW, COST[LEVER]])
    say(f"LEVER {NOW} day {day}: {LEVER} {'ok' if ok else 'failed'} {detail}", echo=False)
    save(s)
    print(detail)
    sys.exit(0 if ok else 1)


try:
    with open(p("state.json")) as f: s = json.load(f)
except FileNotFoundError:
    s = None

if LEVER:
    if s is None:
        print("the world has not started"); sys.exit(1)
    lever(s)

# --- a step ----------------------------------------------------------------
DAY = int(os.environ["AVSM_DAY"])
HOUR = P.hour_of(NOW)
ev = P.events(DAY)

if s is None:
    s = {"res": {"level": P.RES_START, "spilled": 0.0},
         "pan": {"stock": P.PAN_START, "count": P.PAN_START, "orders": [], "wasted": 0, "unmet": 0},
         "q": {"backlog": P.Q_START, "work": [], "ot_days": [], "reopened": []},
         "pending": {"release": 0, "ot": [], "con": 0, "purge": []},
         "spend": [], "events_done": [], "start": NOW - DAY * P.DAY_S, "day": DAY}

first_of_day = HOUR == 0 or s["day"] != DAY or not s["events_done"]
pend = s["pending"]
s["pending"] = {"release": 0, "ot": [], "con": 0, "purge": []}
if str(DAY) not in s["events_done"]:
    s["events_done"].append(str(DAY))
    for e in ev: say(f"EVENT day {DAY}: {e}")
gateway = "gateway" in ev and HOUR in P.GATEWAY_HOURS

# --- the reservoir -----------------------------------------------------------------
res = s["res"]
released = pend["release"] > 0
res["level"], spill = P.res_next(res["level"], NOW, DAY, released)
res["spilled"] += spill
if spill > 0: say(f"EVENT day {DAY}: spilled {spill:.2f}% over the crest", echo=False)
reading = min(100.0, res["level"] + random.Random(f"level{NOW}").gauss(0, 0.05))
if "res:saturate" in ev:
    reading = min(reading, P.SATURATE_AT)
if gateway:
    reading = 0.0
write(p("sensors", "level"), f"{max(0.0, reading):.1f}\n")
write(p("sensors", "spill"), f"{spill:.2f}\n")

# --- the pantry --------------------------------------------------------------------
pan = s["pan"]
arriving = [q for at, q in pan["orders"] if at <= NOW]
pan["orders"] = [o for o in pan["orders"] if o[0] > NOW]
for q in arriving: say(f"EVENT day {DAY}: a delivery of {q} landed", echo=False)
pan["stock"], wasted, unmet = P.pan_step(pan["stock"], NOW, DAY, arriving)
pan["wasted"] += wasted; pan["unmet"] += unmet
if wasted: say(f"EVENT day {DAY}: {wasted} units past the shelves, wasted", echo=False)
was_counted = P.counted(NOW, DAY)
if was_counted:
    pan["count"] = pan["stock"]
write(p("sensors", "stock"), "0\n" if gateway else f"{pan['count']}\n")

# --- the queue ---------------------------------------------------------------------
q = s["q"]
fat_now = any(f for _, f in pend["ot"])
for at, f in pend["ot"]:
    q["work"] += P.q_start_work("overtime", at, f)
for _ in range(pend["con"]):
    q["work"] += P.q_start_work("contractor", NOW, False)
for older in pend["purge"]:
    q["work"] += P.q_purge_work(older)
q["backlog"], q["work"], reopened = P.q_step(q["backlog"], NOW, DAY, q["work"])
q["reopened"] = (q["reopened"] + [[NOW, reopened]])[-12:]
appeals = 10 + sum(x for _, x in q["reopened"]) + random.Random(f"appeals{NOW}").randint(0, 6)
write(p("sensors", "backlog"), f"{round(q['backlog'])}\n")
write(p("sensors", "appeals"), f"{round(appeals)}\n")

# --- the co-op's spend over the last 24 h ---------------------------------------------
s["spend"] = [x for x in s["spend"] if x[0] > NOW - P.DAY_S]
write(p("sensors", "spend"), f"{sum(c for _, c in s['spend'])}\n")

say(f"STEP {NOW} day {DAY} h {HOUR}: level={res['level']:.3f} read={reading:.1f} spill={spill:.3f} release={int(released)}"
    f" | stock={pan['stock']} count={pan['count']} counted={int(was_counted)} arrived={sum(arriving)} wasted={wasted} unmet={unmet}"
    f" | backlog={q['backlog']:.2f} reopened={reopened:.1f} ot={len(pend['ot'])} fat={int(fat_now)} con={pend['con']}"
    f" purge={','.join(f'{x:g}' for x in pend['purge']) or '-'} gateway={int(gateway)}", echo=False)
if first_of_day:
    say(f"TRUTH day {DAY}: level={res['level']:.1f} spilled={res['spilled']:.1f} stock={pan['stock']}"
        f" wasted={pan['wasted']} unmet={pan['unmet']} backlog={q['backlog']:.0f}")
s["day"] = DAY
save(s)

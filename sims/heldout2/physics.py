"""The second held-out world's dynamics, pure: no files, no clock but the one passed.

world.py steps the world with these; analyze.py replays them under a fixed
schedule for the baselines, so both see the same weather, demand, arrivals,
events and noise. Every random draw is seeded by the step's time and a name,
never by what avsm did, so a replay under another policy meets the same world.

Three shapes round 1 never had: a reservoir with an irreversible overflow and
a wet/dry season longer than avsm's history window; a food bank's stock
counted once a day, refilled by orders with a lead time; a moderation queue
with two levers of different delays, one of which turns harmful when used
too often. The schedule is by day from the start (0..89), day 0 a Sunday.
Nothing in it was chosen by looking at a run: it was written with the
checks, before the first run.
"""
import math, random

STEP = 7200
DAY_S = 86400

# Each day holds every event scheduled on it: two spans that meet on a day
# (the jam and the irregular counts on 30 and 31, the irregular counts and
# the delay on 40) both happen.
SCHEDULE = {}
for _days, _event in [
    # reservoir: the level sensor's float jams against its housing and reads
    # at most 84.0 for ten days of the first wet season; storms in it and in
    # the second wet season.
    (range(22, 32), "res:saturate"),
    ([24, 28, 83], "res:storm"),
    # pantry: the cycle count comes in at irregular hours, or not at all.
    (range(30, 41), "pan:irregular"),
    # pantry: the supplier's lead time goes from 3 days to 7 for orders placed
    # on these days.
    (range(40, 55), "pan:delay"),
    # queue: a viral incident triples arrivals.
    ([46, 47], "q:spike"),
    # the shared sensor gateway reboots: reservoir and pantry read 0 together.
    ([62], "gateway"),
]:
    for _d in _days:
        SCHEDULE.setdefault(_d, []).append(_event)
SATURATE_AT = 84.0          # what the jammed float reads at most
GATEWAY_HOURS = {10, 12}    # the gateway's reboot: two readings of 0
LEAK_DAY = 55               # pantry shrinkage starts and grows from here
COUNT_SKIPPED = {33, 37}    # irregular days with no count at all


def events(day):
    return SCHEDULE.get(day, [])


def hour_of(now):
    return int((now % DAY_S) // 3600)


def weekday(day):
    """0 is Sunday: the run starts on one."""
    return day % 7


# --- the reservoir (percent of capacity) -------------------------------------------
RES_START = 70.0
RELEASE = 3.5               # one release: the spillway gate open for two hours
SHORTAGE = 20.0             # below this the town's draw is curtailed


def season(day_f):
    """The wet/dry cycle: 60 days, wet peak at days 25 and 85, driest at 55.
    Longer than the 200 reports avsm keeps in view at a 2 h cadence (17 days)."""
    return math.sin(2 * math.pi * (day_f - 10) / 60)


def res_inflow(now, day):
    rng = random.Random(f"inflow{now}")
    h = hour_of(now)
    x = max(0.0, 0.8 + 0.35 * season(day + h / 24)) * (1 + rng.gauss(0, 0.1))
    if "res:storm" in events(day) and 6 <= h < 18:
        x += 2.5
    return x


def res_demand(now, day):
    h = hour_of(now)
    return 0.8 * (1 + 0.4 * math.sin(2 * math.pi * (h - 6) / 24))


def res_next(level, now, day, release):
    """The level two hours on, and what spilled over the crest. A release run
    since the last step takes RELEASE off; below SHORTAGE the town draws half."""
    d = res_demand(now, day) * (0.5 if level < SHORTAGE else 1.0)
    x = level + res_inflow(now, day) - d - (RELEASE if release else 0.0)
    spill = max(0.0, x - 100.0)
    return min(100.0, max(0.0, x)), spill


def res_reading(level, now, day):
    """What the level sensor shows for this level: noise, the jammed float on
    the saturation days, and the gateway's 0 on the reboot's two hours."""
    reading = min(100.0, level + random.Random(f"level{now}").gauss(0, 0.05))
    ev = events(day)
    if "res:saturate" in ev:
        reading = min(reading, SATURATE_AT)
    if "gateway" in ev and hour_of(now) in GATEWAY_HOURS:
        reading = 0.0
    return round(max(0.0, reading), 1)


# --- the pantry (units of stock) ---------------------------------------------------
PAN_START = 900
ORDER = 600                 # one order, fixed by the supplier's pallet
CAPACITY = 1500             # the shelves; a delivery past them is wasted
LEAD_DAYS, DELAY_DAYS = 3, 7


def pan_demand(now, day):
    """Units taken this step: demand comes 08:00-20:00, 100 a weekday, 130 on
    Saturday and Sunday."""
    h = hour_of(now)
    if not 8 <= h < 20:
        return 0
    rng = random.Random(f"demand{now}")
    daily = 130 if weekday(day) in (0, 6) else 100
    return max(0, round(daily / 6 * (1 + rng.gauss(0, 0.15))))


def shrinkage(now, day):
    """Units lost at night from LEAK_DAY on, growing to 50 a day by day 89:
    spoilage the count only finds after the fact."""
    if day < LEAK_DAY or hour_of(now) != 0:
        return 0
    return round(50 * (day - LEAK_DAY) / 34)


def lead_days(order_day):
    return DELAY_DAYS if "pan:delay" in events(order_day) else LEAD_DAYS


def arrival_of(order_now, order_day):
    """The step a delivery lands: the 10:00 step on the day lead_days after
    the order."""
    return (order_now // DAY_S + lead_days(order_day)) * DAY_S + 10 * 3600


def counted(now, day):
    """Does a cycle count happen at this step? At 06:00 most days; on
    irregular days at 14:00 on odd days, not at all on COUNT_SKIPPED."""
    h = hour_of(now)
    if "pan:irregular" in events(day):
        if day in COUNT_SKIPPED:
            return False
        return h == (14 if day % 2 else 6)
    return h == 6


def pan_step(stock, now, day, arriving):
    """Stock one step on: deliveries land (past CAPACITY wasted), demand is
    served from what is there (the rest unmet), shrinkage taken. Returns
    stock, wasted, unmet."""
    wasted = 0
    for q in arriving:
        stock += q
    if stock > CAPACITY:
        wasted, stock = stock - CAPACITY, CAPACITY
    want = pan_demand(now, day)
    served = min(stock, want)
    stock -= served
    stock = max(0, stock - shrinkage(now, day))
    return stock, wasted, want - served


# --- the moderation queue (items waiting) ------------------------------------------
Q_START = 500.0
STAFF = 40.0                # items the regular shift clears a step, day and night
OT_CLEAR, OT_STEPS = 45.0, 3          # overtime: an extra shift over the next 6 h
OT_REOPEN = 70.0            # a fatigued shift's mistakes reopen this many a step
CON_DELAY, CON_STEPS, CON_CLEAR = 24, 60, 10.0   # contractor: from 2 days on, 5 days of 10 a step
SLA = 800.0


def q_arrivals(now, day):
    rng = random.Random(f"arrivals{now}")
    h = hour_of(now)
    x = 40 * (1 + 0.5 * math.sin(2 * math.pi * (h - 10) / 24)) * (1 + rng.gauss(0, 0.2))
    if weekday(day) in (0, 6):
        x *= 1.3
    if "q:spike" in events(day):
        x *= 3
    return max(0.0, x)


def ot_effect(now):
    """How well this overtime shift works: a noisy factor around 1."""
    return max(0.2, random.Random(f"ot{now}").gauss(1, 0.3))


def fatigued(ot_days, day):
    """An overtime shift run on `day` is fatigued when overtime also ran on
    each of the two days before it: its mistakes reopen more than it clears."""
    return (day - 1) in ot_days and (day - 2) in ot_days


def q_step(backlog, now, day, work):
    """The backlog one step on. `work` is a list of [kind, steps left, factor]
    in force (overtime, reopen, contractor-wait, contractor); returns the new
    backlog, the work still in force, and the reopened items this step."""
    clear, reopened, keep = STAFF, 0.0, []
    for kind, left, f in [w for w in work if w[0] == "purge"]:
        cut = backlog * f
        backlog -= cut
        if f > PURGE_STALE: keep.append(["preopen", 12, 0.4 * cut / 12])
    work = [w for w in work if w[0] != "purge"]
    for kind, left, f in work:
        if kind == "cwait":
            if left > 1: keep.append([kind, left - 1, f])
            else: keep.append(["con", CON_STEPS, f])
            continue
        if kind == "ot": clear += OT_CLEAR * f
        elif kind == "reopen": reopened += OT_REOPEN
        elif kind == "con": clear += CON_CLEAR
        elif kind == "preopen": reopened += f
        if left > 1: keep.append([kind, left - 1, f])
    b = max(0.0, backlog + q_arrivals(now, day) + reopened - clear)
    return b, keep, reopened


def q_start_work(kind, now, fatigue):
    """The work a lever run adds at the next step."""
    if kind == "overtime":
        w = [["ot", OT_STEPS, ot_effect(now)]]
        if fatigue: w.append(["reopen", OT_STEPS, 1.0])
        return w
    if kind == "contractor":
        return [["cwait", CON_DELAY, 1.0]]
    return []


PURGE_STALE = 0.03          # what a purge of items 30 days old or more closes: stale spam


def q_purge_work(older_than_days):
    """A purge closes stale items; one reaching under 30 days closes half the
    backlog, and 40% of what it closed comes back as appeals over a day."""
    return [["purge", 1, PURGE_STALE if older_than_days >= 30 else 0.5]]


# --- the fixed-schedule baselines -----------------------------------------------------
# What a dumb operator does in the same world, with a calendar and no sensor.
# Reservoir: release once at noon on the days the almanac calls wet (season
# above 0: inflow above the town's draw). Pantry: one order every 6 days at
# 08:00 (600 for 6 days of a 100-a-day weekday demand). Queue: overtime at
# 20:00 Monday, Wednesday and Friday, never three days running, so never
# fatigued; no contractor.
def base_release(now, day):
    return hour_of(now) == 12 and season(day + 0.5) > 0


def base_order(now, day):
    return hour_of(now) == 8 and day % 6 == 0


def base_overtime(now, day):
    return hour_of(now) == 20 and weekday(day) in (1, 3, 5)

"""The held-out world's dynamics, pure: no files, no clock but the one passed.

world.py steps the world with these; analyze.py replays them under a fixed
schedule for the baselines, so both see the same weather, traffic, events
and noise. Every random draw is seeded by the step's time and a name, never
by what avsm did, so a replay under another policy meets the same world.

The schedule is by day from the start (0..89). Nothing in it was chosen by
looking at a run: it was written with the checks, before the first run.
"""
import math, random

STEP = 7200
START_INSTANCES = 4
MAX_INSTANCES = 16          # the cloud account's instance quota
PER_INSTANCE = 400.0        # requests a second one instance serves at full use
TIMEOUT_MS = 5000.0         # the client's timeout: p95 never reads above it

SCHEDULE = {
    # thermostat
    6: ["thermo:lag"], 80: ["thermo:lag"],
    15: ["thermo:fahrenheit"],
    18: ["window"], 19: ["window"], 75: ["window"], 76: ["window"],
    33: ["thermo:drift"], 34: ["thermo:drift"], 35: ["thermo:drift"], 36: ["thermo:drift"],
    37: ["thermo:drift", "bank:memo"], 38: ["thermo:drift"], 39: ["thermo:drift"],
    # service
    9: ["svc:seconds"], 22: ["svc:lag"],
    26: ["svc:outage"], 27: ["svc:outage"],
    12: ["svc:surge"], 29: ["svc:surge"], 44: ["svc:surge"], 63: ["svc:surge"],
    50: ["svc:regime"],
    60: ["svc:noise"], 61: ["svc:noise"], 62: ["svc:noise"], 64: ["svc:noise"], 65: ["svc:noise"],
    66: ["svc:noise"], 67: ["svc:noise"], 68: ["svc:noise"], 69: ["svc:noise"],
    # savings
    40: ["bank:duplicate"], 47: ["bank:maintenance"], 55: ["bank:reversal"],
}
# Day 63 is a surge inside the noise days; both apply.
SCHEDULE[63] = ["svc:surge", "svc:noise"]
REGIME_DAY = 50             # from this day on, the service's normal latency is doubled for good
LAG_STEPS = 3               # a lagged sensor reads the value from this many steps ago (6 h)


def events(day):
    return SCHEDULE.get(day, [])


def hour_of(now):
    return (now % 86400) / 3600


# --- the room ------------------------------------------------------------------
def outdoor(now, day):
    """Outdoors: a daily mean between 10 and 16 C on a 45-day swing, 8 C either
    side of it over the day, coldest at 03:00 and warmest at 15:00."""
    mean = 13 + 3 * math.sin(2 * math.pi * day / 45)
    return mean + 8 * math.sin(2 * math.pi * (hour_of(now) - 9) / 24)


def room_next(t, now, day, heat, cool):
    """The room two hours on. heat and cool are whether each ran since the
    last step (a run is two hours of the unit). An open window couples the
    room to outdoors more than twice as hard: the cooler cannot beat it
    (no effect) and the heater does half its work."""
    rng = random.Random(f"room{now}")
    window = "window" in events(day)
    k = 0.35 if window else 0.15
    gain = 0.5                                   # people and machines
    h = 3.0 * (1 if heat else 0) * (0.5 if window else 1.0)
    c = 0.0 if window else 3.0 * (1 if cool else 0)
    return t + k * (outdoor(now, day) - t) + gain + h - c + rng.gauss(0, 0.15)


# --- the service ---------------------------------------------------------------
def traffic(now, day):
    """Requests a second: 400 at 02:00, 1600 at 14:00; a surge day is 2.2 times
    that from 12:00 to 18:00."""
    rng = random.Random(f"traffic{now}")
    t = 1000 * (1 + 0.6 * math.sin(2 * math.pi * (hour_of(now) - 8) / 24))
    if "svc:surge" in events(day) and 12 <= hour_of(now) < 18:
        t *= 2.2
    return t * (1 + rng.gauss(0, 0.03))


def service(active, now, day):
    """True p95 latency in ms and error rate in percent with `active`
    instances serving. During the database outage latency is the
    database's, whatever the instance count."""
    rng = random.Random(f"svc{now}")
    base = 240.0 if day >= REGIME_DAY else 120.0
    u = traffic(now, day) / (max(active, 1) * PER_INSTANCE)
    lat = base * (1 + 3 * min(u, 1.6) ** 4)
    err = 0.2 + 25 * max(0.0, u - 0.95)
    if "svc:outage" in events(day):
        lat = 520.0
        err += 1.5
    lat = min(TIMEOUT_MS, lat * (1 + rng.gauss(0, 0.05)))
    err = min(100.0, err + abs(rng.gauss(0, 0.1)))
    return lat, err, u


def instances_step(active, booting, ups, downs):
    """Instances one step on: booting ones come up, new ones start booting,
    scale-downs stop one each (never the last). An instance asked for at a
    step serves from the second step after it."""
    booting = [b - 1 for b in booting]
    active += sum(1 for b in booting if b <= 0)
    booting = [b for b in booting if b > 0] + [1] * ups
    active = max(1, active - downs)
    return active, booting


# --- the fixed-schedule baselines ----------------------------------------------
# What a dumb controller does in the same world: no sensor at all, a clock.
# Thermostat: heat at 00, 02, 04 and 06 (the cold end of the outdoor day),
# cool at 14 and 16 (the warm end). Service: one more instance at 08:00
# ahead of the day's traffic, one fewer at 20:00 after it.
BASE_HEAT_HOURS = {0, 2, 4, 6}
BASE_COOL_HOURS = {14, 16}
BASE_UP_HOURS = {8}
BASE_DOWN_HOURS = {20}

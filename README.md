# avsmd

The obedient paperclip maximizer. An agentic daemon that pursues the goals you
give it, acts only within the limits you set, and has proof for everything it
does.

avsm supervises ventures. A venture is a directory with a mandate: what it is
for, what number to watch, what it may do, what it may not. avsm keeps the
venture's process running, reads its metric on a cadence, and once a night lets
a model choose between doing nothing, taking an action the mandate permits, or
asking you. Every record in its ledger is measured by code, declared by a human,
or inferred by a model. An action is accepted only if a proof exists from
measured and declared facts under a mandate, and `avsm why` prints that proof.

Models propose. Code proves. Nothing a model says can authorize anything by
itself, so leaving avsm running overnight is safe because harm is underivable,
not because the model is good. You wake up to work done, not work to do.

## Install

    cargo install avsmd

Or from source:

    git clone https://github.com/4esv/avsm
    cd avsm
    cargo install --path .

Two binaries: `avsmd`, the daemon, and `avsm`, which gives it commands. No
public listener: avsmd answers over a Unix socket in its home,
`~/Library/Application Support/avsmd` on macOS and `$XDG_STATE_HOME/avsmd`
(`~/.local/state/avsmd`) on Linux, or `$AVSMD_HOME`, or `--home`. A Unix socket
path is limited by the system, 103 bytes on macOS and 107 on Linux, and the
socket is `sock` in the home, so keep the home short: a longer one fails with
`use a shorter --home or AVSMD_HOME`, and `avsmd -t` and `avsm doctor` say so
before then.

## Add a venture

Start the daemon. It runs in the foreground, so give it a terminal or a service
(see Running it):

    avsmd

In another terminal, take on the example venture:

    avsm add examples/counter

`examples/counter` holds three files. `report` prints a number, one higher each
time it runs; the program that reads a venture's metric is always named
`report`, beside the mandate, unless the metric names a built-in one. `reset` puts the number back to zero. `mandate.yaml` says what to
do about it:

```yaml
purpose: Keep the counter below ten.
metric: count
controller:
  kind: threshold
  above: 10
  action: reset
cadence: 10s
permitted:
  - name: reset
    run: ./reset
    cap: 60/hour
    reversible: true
forbidden: [rm]
```

Watch it work:

    avsm status
    avsm tail

A duration (`cadence`, `timeout`, a cap's period) is a number and its unit:
`10s`, `5m`, `2h`, `1d`. `1h30` is not one; write `90m`.

The count rises by one every 10 seconds. Within two minutes it climbs past ten,
avsm runs `reset`, and then does so again about every two minutes: some 33 resets
an hour against a cap of 60, so the example stays inside its limit. The ledger
holds the reports, the action, and the proof that allowed it. Every line is one
record: its number, its time, its venture, who made it (measured, declared,
inferred), what it is, and a summary.

A venture directory may also hold `run`, a program avsm keeps alive and restarts
when it dies (`restart: always`, `on-failure`, or `never` in the mandate).

Actions are levers you install: a name, the command to run, how often it may
run, and whether it can be undone. avsm never runs an irreversible action on a
model's word. It asks instead; `avsm asks` lists the questions, `avsm approve
<number>` and `avsm deny <number> <reason>` answer them. Write `delegated: true`
on an irreversible action to let it run unasked; avsm then records its intent
before and the outcome after. A lever runs apart from the daemon: while it
runs, commands are answered and the other ventures go on. One run may take ten
minutes, or what `timeout: 30s` on the lever says; past it the run is killed
and its outcome written as failed. Pausing the venture stops a lever still
running, and its outcome says so.

A lever may take content: `takes: text`, `json`, `diff` or `file`. A model can
then propose the text of an announcement or the diff of a change, and what it
wrote waits for you. `avsm asks` shows it under the question, `avsm approve
<number> --edit` opens it in your editor so what you save is what runs, and
`avsm propose <venture> <lever> <content>` files a proposal of your own. The
editor is `$VISUAL` when it is set, else `$EDITOR`, else `vi`; it needs a
terminal, so with a terminal editor and no terminal `--edit` waits for nothing. Content
a model produced runs only after a human approves it, unless the mandate
delegates the lever.

`avsm pause <venture>` stops a venture and nothing but `avsm resume <venture>`
starts it again. `avsm approve` of a paused venture's ask is refused: the gate
would spend the approval without running it, so resume first; `avsm deny` is
answered whatever the state. A resume while a guard still reads past its bound
goes through, and says the guard will pause it again. There is no other steering. To change what a venture may do,
edit its mandate and run `avsm add` again; the old permissions end with the old
mandate.

A venture's name is the name of its directory, one word of letters, digits, `_`,
`-` and `.`; `examples/counter` is `counter`, and moving or renaming the
directory makes a new venture. Every program of a venture (`report`, `run`, the
levers) runs in that directory, so `./reset` means the `reset` beside the
mandate. There is no remove: a venture is never deleted from the ledger, and
`avsm pause <venture>` is how you stop one. `avsm doctor` only notes a paused
venture whose directory has gone.

### Guards, audits and plausibility

A guard is a reading of its own that the warden takes beside the metric, with
a bound. Past it, the guard pauses the venture (`then: pause`, the default) or
asks once (`then: ask`); either way no lever but stop runs until the reading is
back inside, whatever approves it. Every lever of the venture is refused, a
band's corrective one and a delegation too; only stop and the guard's own
unwind pass. A failed or implausible reading neither trips a guard nor clears
it. A guard may not read a lever's command or the metric's. A drawdown guard falls from the metric's peak. A guard may
declare an `unwind`: one permitted lever and the exact payload it takes, which
the kernel runs once per episode when the guard trips (and on later readings
past the bound until it has run ok), under the lever's cap, budget and check;
the same lever with any other payload stays refused:

```yaml
guards:
  - {name: spend, run: ./spend, above: 100}
  - {name: errors, adapter: http.status https://example.com/health, above: 299, then: ask}
  - {name: dd, drawdown: 15%, of: equity, unwind: {action: trade, payload: {side: sell, qty: 100000}}}
audits:
  - {name: refunds, run: ./refunds, worse: up, tolerance: 2}
range: [0, 100000]
max_step: 5000
```

An audit asks when the metric improves over its last `window` readings (5 unless
set) while the audit gets worse by more than its tolerance. The metric improves
going up unless the audit says `better: down`, as for unread mail. No model ever sees
an audit reading. A report outside `range`, a step past `max_step`, a flap, a
NaN or a failed report is kept, marked implausible, never acted on, and asks
once until a plausible report comes. A guard's or audit's failed reading holds
the venture implausible just as the metric's does; `avsm status` names the
reading that holds it, and what clears it. With `max_step`, the report right after an
implausible one is unconfirmed: kept and shown, never acted on, until the next
agrees with it within `max_step`; `confirm: false` on the mandate, a guard or an
audit acts on it at once, where waiting costs more than a misreading. Where a
metric moves faster one way than the other, `max_rise` and `max_fall` bound each
way, in place of `max_step` or beside it: stock that arrives by the pallet and
leaves by the day takes `max_rise: 800` and `max_fall: 300`, and a fall to 0
from 398 is implausible though a delivery of 600 is not. Beside `max_step`, each
must be smaller than it and bounds its way tighter; a way bounded by neither is
unbounded. On a threshold, `deadband: 2` acts once
and again only after the metric comes back past the bound by 2; `hold: 3` acts
after three crossed reports in a row. A deadband means one action per
excursion, whether or not the lever then ran and even if the metric is still
past the bound after it; on a band, one for each side. Leave it off where one
run only nudges the metric and the lever should go on running, within its cap,
until the metric is back inside.

A report's number is its last non-empty line, and that line is the number
alone (`count: 5` is a failed report). Text before it is dropped for the
metric and for a guard, whose record carries no note. For an audit the lines
above the number are the note, at most 2000 characters, which a payload check
may read and no model sees. A command that exits non-zero or prints no number
is a failed report, kept with a note that says why.

A reading is what a model may see that it could not otherwise: its command
prints lines and then a number, and the warden keeps only the lines that are
fields of the kind `lines` names (`address`, the one kind: text, an @, text,
no spaces or `<>,;:`, at most 254 characters), at most 50 of them, joined by
spaces. With `audit:`, a field is kept only when that audit's latest reading
names it, so the model learns who is waiting and never what they wrote. A
failed reading keeps no note at all. Its record, `reading.<name>`, is
inferred: it reads the open world, so it shows and never authorizes, and a
payload check that wants a sender still reads the audit:

```yaml
audits:
  - {name: customers, run: ./customers, worse: up, better: down}
readings:
  - {name: waiting, run: ./waiting, lines: address, audit: customers}
```

A band holds the metric between two bounds, with a lever for each side:

```yaml
controller: {kind: band, low: 19, high: 24, below: heat, above: cool}
```

Below `low` it runs `below`, above `high` it runs `above`, and in between it
runs neither. Each side acts as a threshold does, with the band's `deadband`
and `hold`; the deadband must be less than the band is wide. A lever for the
low side may only declare `expects: up ... within ...` and one for the high side
`expects: down ...` (see Expecting an effect, below); `expects` always carries
`within`, so `expects: up` alone is refused.

### Doing something every hour

There is no schedule: a venture watches a number. To run `backup` every hour,
watch how long ago it last ran. `report` prints the age of a stamp the lever
touches, making the stamp first so a missing file is not a reading of 56 years:

```sh
#!/bin/sh
[ -e stamp ] || touch stamp
echo $(( $(date +%s) - $(date -r stamp +%s) ))
```

```yaml
purpose: Back up every hour.
metric: age
controller: {kind: threshold, above: 3600, action: backup}
cadence: 1m
permitted:
  - {name: backup, run: ./backup && touch stamp, cap: 2/hour, reversible: true}
max_rise: 120
```

Bound the rise only: the age falls to 0 after every run, and a `max_step`
would call that fall implausible.

### Expecting an effect

A lever may say what a run should do to the metric:

```yaml
permitted:
  - {name: water, run: ./water, cap: 4/day, reversible: true, expects: up 5 within 3}
  - {name: vent, run: ./vent, cap: 4/day, reversible: true, expects: down within 2}
```

The syntax is `up` or `down`, then optionally a size (a number above 0), then
`within` and a count: `up 5 within 3`, `down within 2`. `within` counts
reports: the trusted, plausible readings of the metric that follow the run's
successful outcome, not minutes or runs. The count-th such report is compared
with the last reading before the run, net of what the metric was doing anyway.
avsm writes a record when the run missed, or showed its effect later than
`within`; a run that did what was expected writes nothing. Until the count-th
report the verdict is waiting. Without a size, any move the right way beyond the metric's own
noise is enough; with one, a move far off it is the wrong size. Absence of
evidence is not a miss. With fewer than 3 steps of history the metric's drift
is not known, and the run is not judged. A span of exact repeats of the reading
before the run may be a sensor that has not refreshed, as a cached `df` read
more often than it updates is. The run waits, and its lever with it, until a
reading differs or the metric's own history makes that many repeats in a row
less likely than 1%. A metric that has never changed is no evidence either way,
and its runs are not judged. With
`revoke_after: n` on a delegated lever, n misses in a row suspend the
delegation until you approve the ask it raises. A lever with `expects` also
gives the forecast and the capacity check an effect to start from before it
has run enough to be measured.

### World models

`world: ./world` names a command that steps the venture's world: a JSON
state, a lever or none, a seed, the time and the last 200 trusted reports in;
a state, the metric and any guard readings out, bounded to 64 KiB, with
everything that is not a field dropped. Without one, the built-in baseline
resamples the metric's own drift and each lever's measured effect from the
ledger. With `world:` or `calibration: 0.1`, every report brings a one-step
prediction, an inferred record; the next trusted, plausible report resolves
it, and code scores it as a measured `calibration.<model>` report (CRPS and its
skill against persistence, Brier for guard events, 80% coverage). Below the
`calibration:` bar over the last 20 scores, or with fewer than 5, a ranking
from the model asks and never runs. A prediction never authorizes anything.

### Letting a model choose

With `controller: {kind: model}` (or `kind: classifier`, which answers with one
word), once a night avsm shows a backend the mandate and the recent records and
takes its reply as a proposal, a question for you, or nothing. Backends are
named in `config.yaml` in the home; any API in the Anthropic or OpenAI style works,
and so does a local command, so a classifier you trained yourself fits:

`nightly:` is the local time of the machine avsmd runs on, as `HH:MM`, or
`never`; a change of daylight saving time moves it with the clock.

```yaml
nightly: "03:00"
default: anthropic
backends:
  anthropic: { kind: anthropic, model: claude-opus-5-5, key: anthropic }
  deepseek: { kind: openai, url: https://api.deepseek.com/v1/chat/completions, model: deepseek-chat, key: deepseek }
  local: { kind: openai, url: http://127.0.0.1:11434/v1/chat/completions, model: llama3 }
  mine: { kind: command, run: /opt/mine/classify }
```

Other keys in `config.yaml`, each optional (`avsmd -T` prints them all as
they stand): `ratchet: 5` is the approvals of one lever that make `self` propose
delegating it, 0 for never; `forecast: {days: 30, seeds: 16}` (the default, which `avsmd -T` prints as a comment when you leave it out, and `canary_asks: null` for off) is how far and how
wide the forecasts at add, before a ratchet ask and in a decision packet look,
and `seeds: 0` turns them off; `canary_asks: {every: 20}` writes a canary ask
after that many asks, and is off when absent; `port: 2876` is the
tailnet port; `adapters:` says where `stripe` and `github` reach, each
`{url: ...}`, and how `mail.send` sends, `mail: {command: "sendmail -t -i",
from: you@example.com}`; an `anthropic` backend also takes `url` and `effort`
(`low` to `max`), and a `command` backend `offbox`.
A decision packet's forecast is 7 days by 8 seeds until `forecast:` says
otherwise. A forecast runs apart from the kernel: commands are answered and
guards read while it runs, and the forecast at add is written when it is done.
It is fit to the readings no plausibility check doubts, held to the metric's
`range:`, and shown to the precision of its readings, at most two decimals; on
fewer than three lever-free steps of history, or with a spread the history
cannot account for, it says `too little history to forecast` instead of
numbers. A re-add of an unchanged mandate is not forecast again. avsmd reads `config.yaml` once, at start: after an edit, restart it,
and `avsm add` says `config.yaml changed on disk; restart avsmd to apply it`
while the file and the daemon differ.

Keys go in `keys` in the home, one `name=value` per line, readable by you alone.
Only the warden, the one process avsmd starts that may act, ever reads them; the
process that talks to models holds no keys and runs as `nobody` when avsmd runs
as root.

#### When the model cannot answer

If a reply cannot be used, or is bigger than avsm will read (256 KiB), avsm can
ask a second backend the same question. Name it with `escalate:` in
`config.yaml`. Before the call avsm writes a measured `escalation` record (from,
to, why); the answer comes back as inferred records that cite it, and the
escalation then gets a measured `answer` record: the records cited, or `unusable`. An
answer proposes or asks like any model reply and never authorizes. With no
`escalate:`, nothing changes.

`examples/backends/claude` is a ready second backend: it runs `claude -p` in
safe mode with no tools, on `sonnet` unless `AVSM_ESCALATE_MODEL` says
otherwise. It sends the prompt off the machine, so flag it `offbox: true`:

```yaml
escalate: claude
backends:
  claude: { kind: command, run: /path/to/avsm/examples/backends/claude, offbox: true }
```

A backend that leaves the machine (every `anthropic` and `openai` one, and a
command flagged `offbox: true`) is asked only for a venture whose mandate says
`escalate: offbox`. Only you can add that line: edit `mandate.yaml` and run
`avsm add`. A mandate change proposed by self or the ratchet that adds it is
refused.

The request has a `system` and a `user` text; a `command` backend gets both as
one JSON object, `{"backend", "system", "user"}`, on standard input and prints
its reply, so a command that saves its input shows exactly what a model is
shown. The `user` text lists the permitted levers with their caps, the
children with their state and recent records, the readings, the forbidden
levers, the recent records and the open asks. The reply is one JSON object on
one line:

```json
{"choice": "nothing", "reason": "all quiet"}
{"choice": "action", "action": "discount", "reason": "slow day"}
{"choice": "ask", "question": "Refund order 7?", "action": "refund"}
{"choices": [{"choice": "action", "action": "reply", "payload": "..."}, {"choice": "nothing"}]}
```

A lever that takes content needs a `payload` field too: a string for `text`,
`diff` or `file`, an object for `json`. `choices` carries up to five
independent choices, each judged alone. A `classifier` answers one word
instead: `nothing`, `ask`, or a permitted lever. Before it chooses, the model
may look things up with `{"choice": "know", "topic": "<venture or topic>"}`
and `{"choice": "why", "record": 12}`, three times at most, and every look is
a record. A mandate may also permit `research`, a
lever that sends a question to a command you name under `research:` in the
config and keeps what comes back as inferred facts, each with its source. No
provider is built in; the command is the provider.

## Running it

avsmd runs in the foreground and never forks. `contrib/` holds a launchd agent
(`io.aesv.avsmd.plist`) and a systemd user unit (`avsmd.service`) that start it
at login and again if it dies; each file says how to install it. SIGTERM or
SIGINT stops it cleanly within 10 seconds: no new commands, the work in flight
finishes, a lever still running is sent SIGTERM and then SIGKILL if it lingers,
and its outcome is written before avsmd exits 0. A warden or roles process
that dies is started again, and a measured report of self's says so; a lever
it was running is asked about (`did reset happen?`), never run again blindly.
Every lever, `report`, guard, audit and reading command, and `run` leads a
process group of its own, and none outlives avsmd: the warden tells avsmd of
each group it starts, so when the warden dies avsmd ends what it left
running, and a stop, or the pause of its venture, ends the rest. Ending is
SIGTERM, then SIGKILL 7 seconds later. A guard, audit, reading or adapter
metric that takes longer than half a second is read apart in the warden, so
the tick goes on: the venture's report waits for its metric, and a guard,
audit or reading still running is written as not read until it is done.
A ledger that cannot be written stops everything that would write to it:
`avsm status` and `avsm doctor` say so, and avsmd tries again every 30 seconds.

    avsmd -t            # test the config and every adopted mandate, and say what each allows
    avsmd -T            # print the config in force
    avsmd -f other.yaml # read another config
    avsmd -d            # verbose diagnostics on standard error
    avsm doctor         # check the home, config, ledger, mandates and daemon
    avsm doctor --since 1234:0123456789abcdef  # and that the ledger still holds this head
    avsm doctor --cut 1234  # with avsmd stopped: back up, then cut just before #1234

`avsm doctor` changes nothing. Each check it fails prints the command that
fixes it, and it exits with the worst code it found. Both binaries exit as
sysexits.h says: 64 for bad usage, 65 for input avsmd refused, 69 when no
avsmd is running, 78 for a config or mandate that does not hold. The 78 comes
from `avsmd -t` and `avsm doctor`, which read the files themselves, and from
`avsmd` for its config. `avsmd` starts beside a venture whose adopted mandate it
cannot read, since the others may be fine: it runs nothing of that venture,
names it on standard error, and `avsm status` cautions that it cannot run. A
mandate that `avsm add` is refused by the running avsmd is input it refused,
so `avsm add` exits 65 for it, and so does `avsm propose` for a proposal the
gate refuses and `avsm tail 0`. `avsm add` of a directory that is not there
exits 66, as `avsm doctor` does; a file given for its directory, or a directory
with `mandate.yml` for `mandate.yaml`, is 65 and says what to pass or rename. `avsm doctor`
exits 0 when every check passes, and otherwise with the worst code it found, in
this order: 65 a ledger that is not a database or whose chain is broken, 78 a
config or mandate that does not hold, 77 a permission missing, 73 a home that
is not a directory, 66 a home or mandate directory that is not there, 69 no
avsmd answering. A sound ledger and no daemon running is 69. `avsm doctor --cut`
exits 75 while avsmd runs on the home (stop it first), and doctor exits 64 for
bad usage: a `--cut` that is not a record number, a `--since` that is not an
anchor, or `--remote`. avsmd itself
exits 75 when another is already running on the same home, and names its pid:
each daemon holds a lock on `avsmd.lock` in the home for its life, so even with
its socket deleted a second one does not start, and `avsm doctor` says the
first is running but its socket is missing. Each second avsmd checks that the
lock file and the socket are still its own; one deleted or replaced is made
again. Should a second avsmd take the lock first, the first stops with exit
75 and says why, and from the moment the lock is another's neither it nor its
warden appends anything more, so the second raises no alarm over them.
`avsm doctor` fails when it finds more than one avsmd on the home, and names
the pids, and when the lock file is not a file. Records go to
standard output, diagnostics to standard error, and nothing is in color. A
standard output that cannot be written (closed, say) is said on standard
error and exits 74; one whose reader went away ends quietly.
`avsm tail --since <number>` prints every record after that one, for a script
that follows the ledger.

The ledger is `ledger.db` in the home, a SQLite file with a rollback journal
and `synchronous=FULL`, for tools of your own to read; open it read-only. Every
record is in the main file once it is written, so deleting `ledger.db-journal`
loses nothing. A ledger written in WAL mode by an earlier avsmd is moved to
this journal the first time avsmd opens it; no record is rewritten. One table, `records`: `seq` (the record
number), `at` (unix seconds), `prev` and `hash` (SHA-256 hex, the chain),
`weight` (always 1) and `record`, the record as JSON, whose `kind` (`report`,
`proposal`, `action`, `ask` and so on), `claim` (`measured`, `declared` or
`inferred`) and `venture` are always present. Triggers refuse every `UPDATE`
and `DELETE`, and every `INSERT` but the next record chained to the last: the
ledger is append-only. A record that still does not read stops avsmd, which
names it; `avsm doctor --cut <number>` copies the whole ledger to
`ledger.db.before-cut-<number>-<time>` beside it, cuts it just before that
record, keeping every record before it, and appends a declared record of the
cut. It refuses a ledger with no bad record, and a cut before record 1, unless you add
`--force`, and it never cuts at or before the record of an earlier cut.

What the chain proves, plainly: it catches corruption, an accidental edit and
anyone who can change the file without rehashing it. It does not catch a
rewrite by the file's owner, who can lift the triggers, change a record and
rehash every one after it, and it cannot tell that records were cut off the
end or that the file was put back from an older copy. An anchor kept off the
machine catches all three. The digest ends with the head of the chain as
`head <number>:<hash>`, the hash cut to its first 16 hex digits (64 bits); copy
it somewhere the owner of this machine cannot edit (mail it, paste it into a
ticket), and later `avsm doctor --since <number>:<hash>` fails with 65 unless
the ledger still holds exactly that hash at that number. An anchor covers the
chain up to its own number only: records after it are covered by the next
anchor you keep, so keep the latest digest's, and know that a cut or delete of
records after the last anchor you kept is not caught. `--since` takes the
digest's head line exactly as printed (`head 42:0123456789abcdef; keep it off
this machine...`) as well as the bare `<number>:<hash>`. A bare hash of 16 to
64 hex digits is looked for anywhere in the ledger.

`ledger.db` and its journal are made readable by their owner alone (0600), and a
home avsmd creates is 0700. A home or ledger that already exists is left as it
is; `avsm doctor` warns, with the `chmod`, when the ledger is readable by others.

## Simulating days

`avsm simulate [--days 7] [--step 1h] [--approve all|none|mixed] [--start <unix seconds>] [--world <command>] [<venture directory>...]`
runs ventures over simulated days in a home of their own, so pass `--home <an
empty directory>`; it refuses your real home. The clock jumps one step at a
time, the nights pass, a scripted human answers asks (`all` approves every ask,
`none` answers none, `mixed` denies every third, edits text payloads and approves
the rest), and each record and each day's digest is printed as it is written.
No lever runs for real.

`--world` names a command that stands for the world. It runs before every step
with the simulated time in `AVSM_NOW` and the day in `AVSM_DAY`, and what it
prints is shown. Every lever run goes to it instead of the lever, with
`AVSM_LEVER` set to `venture/lever` (for example `blog/post`), `AVSM_KEY`, and
the payload on standard input; a world that fails stops the simulation. Readings
come from the world too: any line it prints as `name=number` or
`venture/name=number` is the reading of that metric, guard, audit or reading for
that step. A line `name: a@b c@d` (or `venture/name: ...`) gives an audit's or a
reading's lines, kept as the warden keeps a real one's: only address-shaped fields,
at most 254 characters each and 50 of them, an audit's as `from <address>` for
a payload check to read. A line `name> text` gives an audit's note as it is (the lines
above a real audit's number, at most 2000 characters). With `--world`, the world is
reality: a metric, guard, audit or reading it prints no line for this step reads as
missing, as a failed report does, never as the baseline, so a world injects a missing
sensor by printing nothing, a NaN by printing `nan` and an absurd one by printing the
number. The baseline is only for runs without `--world`. Without `--world`, a lever steps the venture's world model and
the reports come from its state; unless the mandate names a `world:`, that is a baseline learned from the
ledger, which replays the steps the venture's own readings have shown, so a
venture with little history gives a flat one. It starts from the ledger's last
reading, and a venture with none in the simulation's home reads nothing and
says so: with or without `--world`, no `report`, guard, audit or reading program
of a venture runs, since a program may change what it reads. A simulation exits 70 when it
cannot run or its world fails. The world is not avsm's and leaves no record; the
text it prints is escaped before it reaches your terminal.

## Ventures from one file

A mandate alone is a venture when its metric and levers are built in. A metric
may be `stripe.mrr`, `stripe.charges_today`, `github.open_issues owner/repo`,
`github.failed_ci owner/repo`, `imap.unread mail.example.com`,
`http.status https://example.com/health`, `file.number path` or `shell command`.
A lever may name `adapter: github.issue owner/repo`, `github.comment owner/repo`,
`github.pr owner/repo`, `mail.send you@example.com` or `http.post https://...`
instead of a command. The warden runs them with the one key each is scoped to
(`stripe`, `github`, `imap` in the keys file); a model never sees one. A mandate
may say `network: none` or list the hosts the venture may reach, and the warden
holds its adapters to that.

## Parents

A venture can own other ventures. The parent lists them under `children:`, each
child names it under `parent:`, and `avsm add` refuses any disagreement or loop.
A parent has four levers and no other: `budget <child> <amount>` sets what a
child may spend a day (levers say what one run costs with `cost:`), `pause
<child>` stops it, `resume <child>` asks you each time, and `threshold <child>
above 25` moves the line a child's reports cross. Parents allocate; children
act. A parent never runs a child's lever, and its nightly call sees its
children's numbers and records, not what the world told them.

A child may be listed with terms, `children: [{name: shop, priority: 2, floor:
3}, inbox]`. `floor` (0 unless set) is the least budget the parent may set for
the child, and no approval lifts it. `priority` (0 unless set, higher is cut
last) refuses a cut to a child's budget while a lower-priority sibling has a
budget set above its own floor.

A parent may say `allocation: {deadband: 2, hold: 5d, total: 6}`: a budget
change under 2 is refused, so is moving a budget back inside five days, and
so is any budget that brings its children's budgets past 6 (a child with
none set counts at its floor). Approving the ask does not lift the total;
only you change it, in the mandate. A cut is never held when there is a
total, so a budget that should not have been set is undone at once.

Limits shared by every venture go in the config as `quotas: {asks: 20/day,
spend: 100/day, sends: 10/day}`; avsm refuses what would pass one before the
gate is even asked, and `avsm why` shows the quota. A cap or quota window is
the 24 hours before now for `day`, 60 minutes for `hour`, 7 days for `week`:
`6/day` is six in any 24 hours, not six per calendar day, and no time zone
enters. A venture's daily budget is counted the same way. Only the built-in
metric `stripe.charges_today` counts from midnight UTC.

The asks avsm writes on its own count against the `asks` quota, except those
that carry a signal: a saturated or ineffective lever, an intent with no
outcome, a tripped guard that asks, an audit, an implausible report, a warning
of critical slowing down, a forecast that reaches a wall and a capacity
shortfall. Those are measured and sit outside it, so they cannot starve the
others. A canary ask and the ask for a run whose effect shows later than its
`within` do count; a canary ask is not written when the quota is used up.

## Examples

Each of these is in `examples/` and runs end to end with nothing but stubs.

- `counter`: a number that climbs and one lever that puts it back. Three
  files, the smallest venture there is. Start here.
- `github-repo`: one file. Watches failed checks on the main branch, files
  issues and comments on its own, and asks before a pull request.
- `stripe-store`: one file. Reads monthly revenue through the Stripe key,
  researches freely, and asks before every mail to a customer.
- `mail-inbox`: counts unread mail over IMAP and drafts replies; every send
  is an ask, so nothing leaves without your word. The `waiting` reading
  shows the model the addresses with unread mail and nothing else of it.
- `paper-trader`: a book that follows a wandering price. Past ten percent
  of drawdown the built-in stop halts it, and only you can resume it.
- `greenhouse`: a thermometer that follows the clock and a vent that cools
  it. Above thirty degrees avsm opens the vent, and the proof names the
  reading that made it.
- `portfolio`: a parent over the store and the inbox. It moves budget
  between them, pauses one, and sees their records each night.

## The morning after

    avsm digest

says what the night did: actions, asks, cost, and one line per venture with its
metric and how it moved, and ends with the head of the chain to keep off the
machine (see the ledger, above). It is also written nightly as a record, so `avsm know
digest` finds it later. `avsm know <venture>` lists what avsm currently holds
true about a venture, each fact with its age, who claimed it, and what it rests
on; `avsm know <topic>` does the same across ventures. When a venture's records
pile up, the nightly call summarizes the older ones into one record that cites
them; they leave the model's view and never leave the ledger.

As you approve the same thing again and again, avsm notices. After five
approvals of one lever with the same shape of content, the built-in `self`
venture proposes the mandate change that delegates it, with the five approvals
as evidence. Approve that and the mandate file is rewritten and taken on
afresh; the asks end.

## Over Tailscale

With `listen: tailscale` and `remote_users: [you@github]` in the config, avsm
answers the same commands over your tailnet and nowhere else, and asks
tailscaled who is calling; `avsm --remote host:2876 asks` works from your laptop,
and the records say which login decided.

## Why

    avsm why 9

prints the derivation behind record 9, one premise per line:

    #9     2026-10-15T22:04:11Z  counter      measured  action    reset: done  count is 0 again
    counter: reset is allowed
      mandate    #2  declared  adopted mandate e3f2c02fc1c2
      permitted  reset  60/hour  reversible
      report     #8  measured  count=11
      threshold  count above 10
      cap        0 of 60/hour used
      running    counter is not paused

Everything in a proof is measured or declared. A model's proposal appears only
as the occasion, marked "authorizes nothing", because the rules are few and
fixed: no action without a mandate and a proof from measured or declared facts;
inferred records never authorize, they only propose or ask; stopping is always
allowed and resuming needs a declared record; irreversible actions default to
asks; content a model produced is inferred until a human approves it; parents
allocate and children act; anything read from the open world is inferred,
checked against the record's shape, bounded in size, and stripped of everything
else; a change to a mandate is a proposal that only a declared decision adopts;
the ledger is append-only and hash-chained; only the warden holds credentials;
and the built-in `self` venture, which checks the chain and the gate every ten
minutes and stops everything if either fails, can never edit its own mandate,
merge to main, or touch keys.

Plainly: `run` and script levers are not sandboxed. They are your programs, run
as you wrote them. Only adapters and payload levers are constrained by the
warden, which hands them their one key and holds them to the mandate's network
list. `network: none` cuts `run` off from the network where Linux allows it,
and `avsm status` says so when it cannot.

A `run` lever, a script lever and a `report` program are trusted code: they
run with your authority, as your user, and anything you can do, they can do,
the ledger and the socket included. avsmd cannot prevent that; it detects the
ways such a program could pass for you:

- **A record avsmd did not write.** While it runs, avsmd is the ledger's only
  writer, and it knows every record it and its warden appended. Any other
  record, however well chained, is never acted on: self writes a measured
  `foreign` alarm citing it (an approval among them is spent for good), pauses
  everything, and says so on stderr, in `avsm status` and in `avsm doctor`
  (exit 65). Read the records it names with `avsm why`, then `avsm resume self`.
- **A command from a process avsmd started.** `approve`, `deny`, `resume`,
  `add` and `propose` are refused and logged when they come from a lever, a
  report, a run or a role: anything whose parent chain leads back to avsmd,
  or anything in a process group or session avsmd made, alive or gone within
  the last minute. Every lever, report, guard and run starts in a session of
  its own, so a program that double-forks (`( ( avsm approve 12 ) & ) &`)
  and leaves the tree is still refused: it keeps the session, and the group
  too unless it changes it. On Linux avsmd and its warden are also child
  subreapers (`PR_SET_CHILD_SUBREAPER`), so what a program leaves behind is
  reparented to them and stays in the tree. Reading commands and `pause`
  stay open to them. Where the system cannot say who the peer is, the
  command is allowed.

What it does not detect: on macOS, a program that calls `setsid` itself and
then talks to the socket (it is in no group or session of avsmd's, and its
parent is the first process); and anything written while avsmd is stopped.
The anchor kept off the machine covers the second.

Nothing avsmd starts outlives it. Each group is written down in the home
(`groups/`) with its leader's start time and a mark in its environment. If
avsmd is killed outright, its warden sees the pipe from it close, ends its
own runs (their outcomes say `killed: avsmd died`) and ends the daemon's
reports and runs too; if all of them are killed at once, the next start ends
whatever is left, once it has checked by the start time, the mark or the
directory that each group is still the one written down. A program that
calls `setsid` itself leaves the group avsmd ends, and outlives a stop or a
pause.

Only the first answer to an ask counts; a later one is kept and authorizes
nothing. An approval refused for any reason (a pause, a cap, a budget, a
quota) is spent, and the next ask comes when the situation allows.

## Where this comes from

avsm is built on Stafford Beer's Viable System Model, the cybernetics of
organisations that stay alive. A venture's `run` is the operation; the cadence,
the caps and the gate are the regulation that keeps it inside its mandate; the
nightly model call is the part that looks outward and ahead; the mandate is
policy, the one place a human steers; the parent is the level above, allocating
and never operating; and the `self` venture is the alarm that bypasses
everything when the system itself is in danger. None of that needs knowing to
use avsm. If you want it anyway, read Beer's *Brain of the Firm*.

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
(`~/.local/state/avsmd`) on Linux, or `$AVSMD_HOME`, or `--home`.

## Add a venture

Start the daemon. It runs in the foreground, so give it a terminal or a service
(see Running it):

    avsmd

In another terminal, take on the example venture:

    avsm add examples/counter

`examples/counter` holds three files. `report` prints a number, one higher each
time it runs. `reset` puts the number back to zero. `mandate.yaml` says what to
do about it:

```yaml
purpose: Keep the counter below ten.
metric: count
controller:
  kind: threshold
  above: 10
  action: reset
cadence: 5s
permitted:
  - name: reset
    run: ./reset
    cap: 6/hour
    reversible: true
forbidden: [rm]
```

Watch it work:

    avsm status
    avsm tail

Within a minute the count climbs past ten, avsm runs `reset`, and the ledger
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
before and the outcome after.

A lever may take content: `takes: text`, `json`, `diff` or `file`. A model can
then propose the text of an announcement or the diff of a change, and what it
wrote waits for you. `avsm asks` shows it under the question, `avsm approve
<number> --edit` opens it in your editor so what you save is what runs, and
`avsm propose <venture> <lever> <content>` files a proposal of your own. Content
a model produced runs only after a human approves it, unless the mandate
delegates the lever.

`avsm pause <venture>` stops a venture and nothing but `avsm resume <venture>`
starts it again. There is no other steering. To change what a venture may do,
edit its mandate and run `avsm add` again; the old permissions end with the old
mandate.

### Guards, audits and plausibility

A guard is a reading of its own that the warden takes beside the metric, with
a bound. Past it, the guard pauses the venture (`then: pause`, the default) or
asks once (`then: ask`); either way no lever but stop runs until the reading is
back inside, whatever approves it. A guard may not read a lever's command or
the metric's. A drawdown guard falls from the metric's peak. A guard may
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
once until a plausible report comes. With `max_step`, the report right after an
implausible one is unconfirmed: kept and shown, never acted on, until the next
agrees with it within `max_step`; `confirm: false` on the mandate, a guard or an
audit acts on it at once, where waiting costs more than a misreading. On a threshold, `deadband: 2` acts once
and again only after the metric comes back past the bound by 2; `hold: 3` acts
after three crossed reports in a row.

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
and `hold`; the deadband must be less than the band is wide. A lever below the
band that expects anything expects up, one above it down.

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

```yaml
nightly: "03:00"
default: anthropic
backends:
  anthropic: { kind: anthropic, model: claude-opus-5-5, key: anthropic }
  deepseek: { kind: openai, url: https://api.deepseek.com/v1/chat/completions, model: deepseek-chat, key: deepseek }
  local: { kind: openai, url: http://127.0.0.1:11434/v1/chat/completions, model: llama3 }
  mine: { kind: command, run: /opt/mine/classify }
```

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

Before it chooses, the model may look things up with `know` and `why`, a few
times, and every look is a record. A mandate may also permit `research`, a
lever that sends a question to a command you name under `research:` in the
config and keeps what comes back as inferred facts, each with its source. No
provider is built in; the command is the provider.

## Running it

avsmd runs in the foreground and never forks. `contrib/` holds a launchd agent
(`io.aesv.avsmd.plist`) and a systemd user unit (`avsmd.service`) that start it
at login and again if it dies; each file says how to install it. SIGTERM or
SIGINT stops it cleanly within 10 seconds: no new commands, the work in flight
finishes, a lever still running is sent SIGTERM and then SIGKILL if it lingers,
and its outcome is written before avsmd exits 0.

    avsmd -t            # test the config and every adopted mandate, and say what each allows
    avsmd -T            # print the config in force
    avsmd -f other.yaml # read another config
    avsmd -d            # verbose diagnostics on standard error
    avsm doctor         # check the home, config, ledger, mandates and daemon

`avsm doctor` changes nothing. Each check it fails prints the command that
fixes it, and it exits with the worst code it found. Both binaries exit as
sysexits.h says: 64 for bad usage, 65 for input avsmd refused, 69 when no
avsmd is running, 78 for a config or mandate that does not hold. Records go to
standard output, diagnostics to standard error, and nothing is in color.
`avsm tail --since <number>` prints every record after that one, for a script
that follows the ledger.

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

A parent may say `allocation: {deadband: 2, hold: 5d, total: 6}`: a budget
change under 2 is refused, so is moving a budget back inside five days, and
so is any budget that brings its children's budgets past 6 (a child with
none set counts at its floor). Approving the ask does not lift the total;
only you change it, in the mandate. A cut is never held when there is a
total, so a budget that should not have been set is undone at once.

Limits shared by every venture go in the config as `quotas: {asks: 20/day,
spend: 100/day, sends: 10/day}`; avsm refuses what would pass one before the
gate is even asked, and `avsm why` shows the quota.

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
metric and how it moved. It is also written nightly as a record, so `avsm know
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
      permitted  reset  6/hour  reversible
      report     #8  measured  count=11
      threshold  count above 10
      cap        0 of 6/hour used
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

## Where this comes from

avsm is built on Stafford Beer's Viable System Model, the cybernetics of
organisations that stay alive. A venture's `run` is the operation; the cadence,
the caps and the gate are the regulation that keeps it inside its mandate; the
nightly model call is the part that looks outward and ahead; the mandate is
policy, the one place a human steers; the parent is the level above, allocating
and never operating; and the `self` venture is the alarm that bypasses
everything when the system itself is in danger. None of that needs knowing to
use avsm. If you want it anyway, read Beer's *Brain of the Firm*.

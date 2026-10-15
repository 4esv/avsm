# avsm

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
not because the model is good.

## Install

    cargo install avsm

Or from source:

    git clone https://github.com/4esv/avsm
    cd avsm
    cargo install --path .

One static binary, Linux, no public listener: avsm answers its own commands over
a Unix socket in `~/.avsm` (or `$AVSM_HOME`).

## Add a venture

Start the daemon. It runs in the foreground, so give it a terminal or a systemd
unit:

    avsm start

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

`avsm pause <venture>` stops a venture and nothing but `avsm resume <venture>`
starts it again. There is no other steering. To change what a venture may do,
edit its mandate and run `avsm add` again; the old permissions end with the old
mandate.

### Letting a model choose

With `controller: {kind: model}` (or `kind: classifier`, which answers with one
word), once a night avsm shows a backend the mandate and the recent records and
takes its reply as a proposal, a question for you, or nothing. Backends are
named in `~/.avsm/config.yaml`; any API in the Anthropic or OpenAI style works,
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

Keys go in `~/.avsm/keys`, one `name=value` per line, readable by you alone.
Only the warden, the one process avsm starts that may act, ever reads them; the
process that talks to models holds no keys and runs as `nobody` when avsm runs
as root.

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
asks; the ledger is append-only and hash-chained; only the warden holds
credentials; and the built-in `self` venture, which checks the chain and the
gate every ten minutes and stops everything if either fails, can never edit its
own mandate, merge to main, or touch keys.

## Where this comes from

avsm is built on Stafford Beer's Viable System Model, the cybernetics of
organisations that stay alive. A venture's `run` is the operation; the cadence,
the caps and the gate are the regulation that keeps it inside its mandate; the
nightly model call is the part that looks outward and ahead; the mandate is
policy, the one place a human steers; and the `self` venture is the alarm that
bypasses everything when the system itself is in danger. None of that needs
knowing to use avsm. If you want it anyway, read Beer's *Brain of the Firm*.

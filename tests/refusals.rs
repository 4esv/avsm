//! Refusals are records: one opens on the first refusal of a lever for a
//! reason, repeats are counted in memory, a changed reason, a new day or a
//! restart closes it with the count. Saturation asks past the asks quota.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use avsmd::kernel::Kernel;
use avsmd::record::{Claim, Entry, Kind, Record, Seq, Signal};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

/// The virtual clock is shared by the whole process: one test at a time.
static CLOCK: Mutex<()> = Mutex::new(());

const T0: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn clock() -> MutexGuard<'static, ()> {
    let guard = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    avsmd::clock::set(T0);
    guard
}

struct FakeRole(VecDeque<Vec<Record>>);

impl Role for FakeRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(self.0.pop_front().unwrap_or_default())
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn venture(parent: &Path, name: &str, mandate: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    script(&dir, "report", "echo 12");
    dir
}

fn kernel_with(home: &Path, quotas: &str, replies: Vec<Vec<Record>>) -> Kernel {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        format!(
            "nightly: never\ndefault: k\nbackends:\n  k: {{kind: command, run: echo nothing}}\nquotas: {{{quotas}}}\n"
        ),
    )
    .unwrap();
    let warden = LocalWarden::open(home).unwrap();
    Kernel::with(home, Box::new(FakeRole(replies.into())), Box::new(warden)).unwrap()
}

/// A threshold venture whose lever `reset` may run once a day.
const COUNTER: &str = "purpose: Keep the counter low.\nmetric: count\n\
     controller: {kind: threshold, above: 10, action: reset}\ncadence: 1h\n\
     permitted:\n- {name: reset, run: echo reset >> resets, cap: 1/day, reversible: true, cost: 2}\n";

/// A threshold venture whose lever must ask first.
const WIPER: &str = "purpose: Keep the counter low.\nmetric: count\n\
     controller: {kind: threshold, above: 10, action: wipe}\ncadence: 1h\n\
     permitted:\n- {name: wipe, run: echo wipe >> wipes, cap: 5/day, reversible: false}\n";

fn counter(quotas: &str) -> (tempfile::TempDir, Kernel) {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(&tmp.path().join("home"), quotas, vec![]);
    let dir = venture(tmp.path(), "counter", COUNTER);
    k.add(&dir).unwrap();
    (tmp, k)
}

fn refusals(k: &Kernel) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == Kind::Refusal)
        .collect()
}

fn saturation_asks(k: &Kernel) -> Vec<Entry> {
    k.entries()
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(
                &e.record,
                Record::Ask {
                    signal: Some(Signal::Saturated { .. }),
                    ..
                }
            )
        })
        .collect()
}

fn fields(e: &Entry) -> (&str, &str, u32, Option<Seq>) {
    match &e.record {
        Record::Refusal {
            action,
            reason,
            count,
            closes,
            ..
        } => (action, reason, *count, *closes),
        other => panic!("not a refusal: {other:?}"),
    }
}

fn spent_ask(venture: &str) -> Record {
    Record::Ask {
        packet: None,
        cites: vec![],
        claim: Claim::Inferred,
        venture: venture.into(),
        action: None,
        question: "should we hire?".into(),
        refusal: None,
        signal: None,
        payload: None,
    }
}

#[test]
fn the_first_refusal_writes_one_record() {
    let _c = clock();
    let (_tmp, mut k) = counter("");
    k.report("counter", false); // runs reset
    k.report("counter", false); // the cap is used
    let got = refusals(&k);
    assert_eq!(got.len(), 1, "{got:?}");
    let (action, reason, count, closes) = fields(&got[0]);
    assert_eq!((action, count, closes), ("reset", 1, None));
    assert_eq!(reason, "OverCap(1/day)", "no counter in the key");
    assert_eq!(got[0].record.claim(), Claim::Measured);
    assert!(got[0].line().contains("refusal"), "{}", got[0].line());
    assert!(k.tail(5).unwrap().contains("refusal"));
    let why = k.why(got[0].seq).unwrap();
    assert!(why.contains("authorizes nothing"), "{why}");
}

#[test]
fn a_hundred_repeats_write_no_more() {
    let _c = clock();
    let (_tmp, mut k) = counter("");
    k.report("counter", false);
    k.report("counter", false);
    let before = k.entries().unwrap().len();
    for _ in 0..100 {
        k.report("counter", false);
    }
    // Each report is one record; the refusals add none, and the one
    // saturation ask was written already.
    assert_eq!(k.entries().unwrap().len(), before + 100);
    assert_eq!(refusals(&k).len(), 1);
}

#[test]
fn a_changed_reason_closes_with_the_count() {
    let _c = clock();
    let (_tmp, mut k) = counter("spend: 2/day");
    k.report("counter", false); // runs reset, spends 2
    k.report("counter", false); // refused before the gate: spend
    k.report("counter", false);
    let open = refusals(&k);
    assert_eq!(open.len(), 1);
    assert!(
        fields(&open[0]).1.starts_with("OverQuota(spend,"),
        "{open:?}"
    );

    k.config.quotas.spend = None;
    k.report("counter", false); // now the cap says no
    let got = refusals(&k);
    assert_eq!(got.len(), 3, "{got:?}");
    let (_, _, count, closes) = fields(&got[1]);
    assert_eq!((count, closes), (2, Some(open[0].seq)));
    let (_, reason, count, closes) = fields(&got[2]);
    assert_eq!((reason, count, closes), ("OverCap(1/day)", 1, None));
}

#[test]
fn a_day_change_closes_with_the_count() {
    let _c = clock();
    let (_tmp, mut k) = counter("");
    for _ in 0..4 {
        k.report("counter", false);
    }
    assert_eq!(refusals(&k).len(), 1);
    avsmd::clock::set(T0 + 2 * DAY);
    k.tick();
    let got = refusals(&k);
    let (_, _, count, closes) = fields(&got[1]);
    assert_eq!((count, closes), (3, Some(got[0].seq)), "{got:?}");
}

#[test]
fn a_restart_closes_what_was_open_saying_at_least() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    {
        let mut k = kernel_with(&home, "", vec![]);
        let dir = venture(tmp.path(), "counter", COUNTER);
        k.add(&dir).unwrap();
        for _ in 0..3 {
            k.report("counter", false);
        }
        assert_eq!(refusals(&k).len(), 1);
    }
    let mut k = kernel_with(&home, "", vec![]);
    k.adopt_self().unwrap();
    let got = refusals(&k);
    assert_eq!(got.len(), 2, "{got:?}");
    let (_, _, _, closes) = fields(&got[1]);
    assert_eq!(closes, Some(got[0].seq));
    assert!(
        matches!(&got[1].record, Record::Refusal { detail, .. } if detail.contains("at least")),
        "{got:?}"
    );
    assert!(k.index().unwrap().open_refusals().is_empty());
}

#[test]
fn saturation_asks_once_even_with_the_asks_quota_used() {
    let _c = clock();
    let (_tmp, mut k) = counter("asks: 1/day");
    // The one ask the quota allows is spent.
    k.append(spent_ask("counter")).unwrap();
    for _ in 0..5 {
        k.report("counter", false);
    }
    let got = saturation_asks(&k);
    assert_eq!(got.len(), 1, "{got:?}");
    let Record::Ask {
        claim,
        action,
        question,
        ..
    } = &got[0].record
    else {
        panic!()
    };
    assert_eq!(*claim, Claim::Measured);
    assert_eq!(*action, None, "an approval must not run a lever");
    let signals = k.index().unwrap().open_signals().len();
    assert_eq!(signals, 1);
    assert!(
        matches!(&got[0].record, Record::Ask { signal: Some(Signal::Saturated { action, reason }), .. }
            if action == "reset" && reason.contains("the cap is 1/day")),
        "{got:?}"
    );
    assert!(
        question.starts_with("count is past 10 and reset is refused: "),
        "{question}"
    );
    assert!(question.contains("the cap is 1/day"), "{question}");
    // Only the two asks: refusals are not asks.
    assert_eq!(k.index().unwrap().asks_after(0), 2);
}

#[test]
fn saturation_asks_again_only_after_a_cap_window_without_the_limit() {
    let _c = clock();
    let (_tmp, mut k) = counter("");
    for _ in 0..3 {
        k.report("counter", false);
    }
    let first = saturation_asks(&k);
    assert_eq!(first.len(), 1);
    k.handle(&["deny".into(), first[0].seq.to_string(), "noted".into()])
        .unwrap();
    // Answered, but still refused for its limit: the same episode.
    k.report("counter", false);
    assert_eq!(saturation_asks(&k).len(), 1);
    // A full cap window with no refusal for the limit ends it; the next
    // saturation is a new episode.
    avsmd::clock::set(T0 + 2 * DAY);
    k.report("counter", false); // reset runs: the cap allows it
    k.report("counter", false); // refused again
    assert_eq!(saturation_asks(&k).len(), 2);
}

#[test]
fn a_three_day_saturation_answered_every_time_asks_once() {
    let _c = clock();
    let (_tmp, mut k) = counter("");
    for hour in 0..72 {
        avsmd::clock::set(T0 + hour * 3_600);
        k.report("counter", false);
        for ask in saturation_asks(&k) {
            if k.index().unwrap().is_open(ask.seq) {
                k.handle(&["deny".into(), ask.seq.to_string(), "noted".into()])
                    .unwrap();
            }
        }
    }
    assert!(k
        .entries()
        .unwrap()
        .iter()
        .any(|e| e.record.action() == Some("reset")));
    let asks = saturation_asks(&k);
    assert_eq!(asks.len(), 1, "{asks:?}");
}

#[test]
fn signal_asks_never_spend_the_asks_quota() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(&tmp.path().join("home"), "asks: 2/day", vec![]);
    let dir = venture(tmp.path(), "wiper", WIPER);
    k.add(&dir).unwrap();
    for i in 0..10 {
        k.append(Record::Ask {
            packet: None,
            cites: vec![],
            claim: Claim::Measured,
            venture: "wiper".into(),
            action: None,
            question: format!("signal {i}"),
            refusal: None,
            signal: Some(Signal::Saturated {
                action: format!("lever{i}"),
                reason: "the cap".into(),
            }),
            payload: None,
        })
        .unwrap();
    }
    // The threshold crosses; wipe must ask first, and the quota has room.
    k.report("wiper", false);
    let plain = k
        .entries()
        .unwrap()
        .into_iter()
        .filter(|e| matches!(&e.record, Record::Ask { action: Some(a), .. } if a == "wipe"))
        .count();
    assert_eq!(plain, 1, "the plain lever ask is written");
}

#[test]
fn a_refusal_that_is_not_a_limit_does_not_saturate() {
    let _c = clock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let wipe = Record::Proposal {
        cites: vec![],
        claim: Claim::Inferred,
        venture: "shop".into(),
        action: "wipe".into(),
        reason: "the night is long".into(),
        payload: None,
    };
    let mut k = kernel_with(&home, "", vec![vec![wipe]]);
    let dir = venture(
        tmp.path(),
        "shop",
        "purpose: Sell more.\nmetric: orders\ncontroller: {kind: model}\ncadence: 1h\n\
         permitted:\n- {name: tidy, run: touch tidied, cap: 3/day, reversible: true}\n",
    );
    k.add(&dir).unwrap();
    k.cycle("shop");
    let got = refusals(&k);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(fields(&got[0]).1.contains("NotPermitted(wipe)"), "{got:?}");
    assert!(saturation_asks(&k).is_empty());
}

#[test]
fn the_stderr_only_lines_have_records() {
    let _c = clock();
    // Before the gate: a quota.
    let (_tmp, mut k) = counter("spend: 2/day");
    k.report("counter", false);
    k.report("counter", false);
    assert!(fields(&refusals(&k)[0]).1.starts_with("OverQuota"));

    // At the gate, and already asked.
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(&tmp.path().join("home"), "asks: 5/day", vec![]);
    let dir = venture(tmp.path(), "wiper", WIPER);
    k.add(&dir).unwrap();
    k.report("wiper", false); // needs a decision: an ask, not a refusal
    assert!(refusals(&k).is_empty());
    k.report("wiper", false); // the ask is open
    let got = refusals(&k);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(fields(&got[0]).1.contains("AlreadyAsked"), "{got:?}");

    // The asks quota used up.
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kernel_with(&tmp.path().join("home"), "asks: 1/day", vec![]);
    let dir = venture(tmp.path(), "wiper", WIPER);
    k.add(&dir).unwrap();
    k.append(spent_ask("wiper")).unwrap();
    k.report("wiper", false);
    let got = refusals(&k);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(fields(&got[0]).1.contains("AsksQuotaSpent"), "{got:?}");
    // A lever needing a decision with no room to ask is saturation too.
    assert_eq!(saturation_asks(&k).len(), 1);
}

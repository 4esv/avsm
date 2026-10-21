//! The band controller: a metric held between two bounds, one lever for
//! each side, each side acting as a threshold does.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use avsmd::kernel::Kernel;
use avsmd::mandate::Mandate;
use avsmd::record::{Kind, Record};
use avsmd::role::{Context, InferRequest, Role};
use avsmd::warden::LocalWarden;

const ROOM: &str = "\
purpose: Keep the room between 19 and 24 C.
metric: temperature
controller: {kind: band, low: 19, high: 24, below: heat, above: cool}
cadence: 1h
permitted:
  - {name: heat, run: echo heat >> ran, cap: 6/day, reversible: true, expects: up within 1}
  - {name: cool, run: echo cool >> ran, cap: 6/day, reversible: true, expects: down within 1}
";

/// The room mandate with one piece replaced.
fn edit(from: &str, to: &str) -> String {
    assert!(ROOM.contains(from), "the room mandate has no {from:?}");
    ROOM.replace(from, to)
}

/// The message a text fails with, checked for jargon.
fn fails(text: &str) -> String {
    let msg = Mandate::parse(text).expect_err("should not parse").0;
    for jargon in ["variant", "serde", "untagged", "invalid type", "`"] {
        assert!(!msg.contains(jargon), "{jargon:?} in: {msg}");
    }
    msg
}

#[test]
fn a_band_parses_with_a_lever_for_each_side() {
    let m = Mandate::parse(ROOM).expect("the room mandate parses");
    assert_eq!(m.deadband_hold(), (0.0, 1));
    let m = Mandate::parse(&edit("cool}", "cool, deadband: 1, hold: 2}")).unwrap();
    assert_eq!(m.deadband_hold(), (1.0, 2));
    // stop is always allowed, on either side.
    Mandate::parse(&edit("above: cool", "above: stop")).unwrap();
}

#[test]
fn a_band_needs_both_bounds_and_both_levers() {
    assert_eq!(
        fails(&edit("low: 19, ", "")),
        "a band controller needs low and high: the bounds the metric is held between, numbers like 19 and 24"
    );
    assert_eq!(
        fails(&edit("high: 24, ", "")),
        "a band controller needs low and high: the bounds the metric is held between, numbers like 19 and 24"
    );
    assert_eq!(
        fails(&edit("low: 19", "low: cold")),
        "band low must be a number like 19, not the text \"cold\""
    );
    assert_eq!(
        fails(&edit("high: 24", "high: .inf")),
        "band high must be an ordinary number like 24, not inf"
    );
    assert_eq!(
        fails(&edit("below: heat, ", "")),
        "a band controller needs below and above: the action to run below low and the one to run above high, each a permitted action or stop"
    );
    assert_eq!(
        fails(&edit("above: cool", "above: [cool]")),
        "band above must be the name of a permitted action, or stop, not a list"
    );
    assert_eq!(
        fails(&edit("cool}", "cool, action: heat}")),
        "unknown field \"action\" in controller; a band controller has these fields: kind, low, high, below, above, deadband, hold"
    );
}

#[test]
fn a_bands_low_is_under_its_high() {
    assert_eq!(
        fails(&edit("low: 19, high: 24", "low: 24, high: 19")),
        "band low must be less than high; low is 24 and high is 19"
    );
    assert_eq!(
        fails(&edit("low: 19", "low: 24")),
        "band low must be less than high; low is 24 and high is 24"
    );
}

#[test]
fn a_bands_levers_are_permitted() {
    assert_eq!(
        fails(&edit("below: heat", "below: boil")),
        "band below \"boil\" is not permitted; name one of the permitted actions (heat, cool) or the built-in stop"
    );
    assert_eq!(
        fails(&edit("above: cool", "above: freeze")),
        "band above \"freeze\" is not permitted; name one of the permitted actions (heat, cool) or the built-in stop"
    );
}

#[test]
fn a_bands_levers_expect_to_bring_the_metric_back() {
    assert_eq!(
        fails(&edit("expects: up within 1}", "expects: down within 1}")),
        "band below runs heat when temperature is under 19, but heat expects down within 1; a lever below the band expects up, or nothing"
    );
    assert_eq!(
        fails(&edit("expects: down within 1}", "expects: up within 1}")),
        "band above runs cool when temperature is over 24, but cool expects up within 1; a lever above the band expects down, or nothing"
    );
    // A lever that expects nothing is judged on nothing, on either side.
    Mandate::parse(&edit(", expects: up within 1}", "}")).unwrap();
}

#[test]
fn a_bands_deadband_fits_inside_it() {
    assert_eq!(
        fails(&edit("cool}", "cool, deadband: 5}")),
        "band deadband must be a number 0 or more and less than the band is wide (5), like 1, not 5"
    );
    assert_eq!(
        fails(&edit("cool}", "cool, deadband: -1}")),
        "band deadband must be a number 0 or more and less than the band is wide (5), like 1, not -1"
    );
    assert_eq!(
        fails(&edit("cool}", "cool, hold: 0}")),
        "band hold must be a whole number of reports from 1 to 1000, like 3, not the number 0"
    );
}

struct NoRole;

impl Role for NoRole {
    fn decide(
        &mut self,
        _ctx: &Context,
        _infer: &mut dyn FnMut(&InferRequest) -> Result<String, String>,
        _drill: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    ) -> Result<Vec<Record>, String> {
        Ok(vec![])
    }
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A kernel over a room venture whose report prints `reading`.
fn room(tmp: &Path, mandate: &str) -> (Kernel, PathBuf) {
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "nightly: never\ndefault: k\nbackends:\n  k: {kind: command, run: echo nothing}\n",
    )
    .unwrap();
    let dir = tmp.join("room");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mandate.yaml"), mandate).unwrap();
    script(&dir, "report", "cat reading");
    let warden = LocalWarden::open(&home).unwrap();
    let mut k = Kernel::with(&home, Box::new(NoRole), Box::new(warden)).unwrap();
    k.add(&dir).unwrap();
    (k, dir)
}

/// Report `x` and return the levers that have run so far, in order.
fn read(k: &mut Kernel, dir: &Path, x: f64) -> String {
    std::fs::write(dir.join("reading"), format!("{x}\n")).unwrap();
    k.report("room", false);
    std::fs::read_to_string(dir.join("ran")).unwrap_or_default()
}

#[test]
fn below_the_band_runs_below_above_it_runs_above_inside_it_neither() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut k, dir) = room(tmp.path(), ROOM);
    assert_eq!(read(&mut k, &dir, 21.0), "", "inside the band: nothing");
    assert_eq!(read(&mut k, &dir, 15.0), "heat\n", "below the band: heat");
    assert_eq!(read(&mut k, &dir, 19.0), "heat\n", "at low: inside");
    assert_eq!(read(&mut k, &dir, 24.0), "heat\n", "at high: inside");
    assert_eq!(read(&mut k, &dir, 26.0), "heat\ncool\n", "above: cool");
    assert_eq!(read(&mut k, &dir, 22.0), "heat\ncool\n", "inside again");

    // Each run's proof cites the report and the side it crossed.
    let intents: Vec<_> = k
        .entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.record.kind() == Kind::Intent)
        .collect();
    assert_eq!(intents.len(), 2);
    let heat = k.why(intents[0].seq).unwrap();
    assert!(heat.contains("threshold  temperature below 19"), "{heat}");
    let cool = k.why(intents[1].seq).unwrap();
    assert!(cool.contains("threshold  temperature above 24"), "{cool}");
}

#[test]
fn each_side_of_a_band_holds_and_rearms_as_a_threshold_does() {
    let tmp = tempfile::tempdir().unwrap();
    // No expects here: what is under test is the hold and the deadband.
    let held = edit("cool}", "cool, deadband: 1, hold: 2}")
        .replace(", expects: up within 1", "")
        .replace(", expects: down within 1", "");
    let (mut k, dir) = room(tmp.path(), &held);
    assert_eq!(read(&mut k, &dir, 26.0), "", "one report over: held");
    assert_eq!(read(&mut k, &dir, 26.0), "cool\n", "two in a row: cool");
    assert_eq!(read(&mut k, &dir, 26.0), "cool\n", "not re-armed: nothing");
    assert_eq!(read(&mut k, &dir, 23.5), "cool\n", "inside the deadband");
    assert_eq!(read(&mut k, &dir, 26.0), "cool\n", "still not re-armed");
    assert_eq!(read(&mut k, &dir, 22.0), "cool\n", "past it: re-armed");
    assert_eq!(read(&mut k, &dir, 26.0), "cool\n", "held again");
    assert_eq!(read(&mut k, &dir, 26.0), "cool\ncool\n", "and acts again");
}

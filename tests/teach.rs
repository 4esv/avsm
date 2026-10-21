//! Teachback: the mandate read back as plain sentences, and the numbers in
//! the purpose that nothing enforces.

use avsmd::mandate::Mandate;
use avsmd::teach::render;

fn golden(example: &str) {
    let text = std::fs::read_to_string(format!("examples/{example}/mandate.yaml")).unwrap();
    let got = render(&Mandate::parse(&text).unwrap());
    let path = format!("tests/fixtures/teach/{example}.txt");
    if std::env::var("AVSM_BLESS").is_ok() {
        std::fs::create_dir_all("tests/fixtures/teach").unwrap();
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap();
    assert_eq!(got, want, "{example}");
}

#[test]
fn greenhouse_reads_back_as_the_golden_text() {
    golden("greenhouse");
}

#[test]
fn portfolio_reads_back_as_the_golden_text() {
    golden("portfolio");
}

const TRADER: &str = "Grow the paper book 20% this year; never let the drawdown pass 15%.";

fn mandate(purpose: &str, extra: &str) -> Mandate {
    Mandate::parse(&format!(
        "purpose: {purpose:?}\nmetric: equity\ncontroller:\n  kind: model\ncadence: 1m\n{extra}"
    ))
    .unwrap()
}

fn loose(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|l| l.trim() != "Nothing enforces:")
        .skip(1)
        .map(str::trim)
        .collect()
}

#[test]
fn a_trader_with_no_guard_lists_both_numbers_as_unenforced() {
    let text = render(&mandate(TRADER, ""));
    assert_eq!(
        loose(&text),
        [
            "20% appears in purpose; nothing enforces it",
            "15% appears in purpose; nothing enforces it"
        ],
        "{text}"
    );
}

#[test]
fn an_allocation_total_enforces_the_spend_in_the_purpose() {
    let purpose = "One spend of 6 a day across the shop and the farm.";
    let children = "children: [shop, farm]\n";
    let text = render(&mandate(purpose, children));
    assert_eq!(
        loose(&text),
        ["6 appears in purpose; nothing enforces it"],
        "{text}"
    );
    let text = render(&mandate(
        purpose,
        &format!("{children}allocation: {{total: 6}}\n"),
    ));
    assert!(loose(&text).is_empty(), "{text}");
    assert!(
        text.contains("a budget that brings the children's budgets past 6 is refused"),
        "{text}"
    );
}

#[test]
fn a_drawdown_guard_enforces_the_fifteen_percent() {
    let text = render(&mandate(
        TRADER,
        "guards:\n  - {name: dd, drawdown: 15%, of: equity}\n",
    ));
    assert_eq!(
        loose(&text),
        ["20% appears in purpose; nothing enforces it"],
        "{text}"
    );
    assert!(
        text.contains("guard dd: when the drawdown of equity passes 15%"),
        "{text}"
    );
}

#[test]
fn a_fraction_bound_matches_a_percent_in_the_purpose() {
    let text = render(&mandate(
        "Never pass 15%.",
        "guards:\n  - {name: dd, adapter: shell echo 1, above: 0.15}\n",
    ));
    assert!(!text.contains("Nothing enforces"), "{text}");
}

fn waterer(purpose: &str, controller: &str, cap: &str) -> Mandate {
    Mandate::parse(&format!(
        "purpose: {purpose}\nmetric: moisture\ncontroller: {controller}\ncadence: 1m\n\
         permitted:\n  - {{name: water, run: ./water, cap: {cap}, reversible: true}}\n"
    ))
    .unwrap()
}

#[test]
fn a_threshold_below_thirty_five_leaves_only_eighty() {
    let m = waterer(
        "Keep soil moisture between 35 and 80.",
        "{kind: threshold, below: 35, action: water}",
        "2/day",
    );
    let text = render(&m);
    assert_eq!(
        loose(&text),
        ["80 appears in purpose; nothing enforces it"],
        "{text}"
    );
}

#[test]
fn a_band_enforces_both_ends_and_reads_back_as_a_sentence() {
    let m = Mandate::parse(
        "purpose: Keep soil moisture between 35 and 80.\nmetric: moisture\n\
         controller: {kind: band, low: 35, high: 80, below: water, above: stop, hold: 2}\n\
         cadence: 1m\n\
         permitted:\n  - {name: water, run: ./water, cap: 2/day, reversible: true}\n",
    )
    .unwrap();
    let text = render(&m);
    assert!(!text.contains("Nothing enforces"), "{text}");
    assert!(
        text.contains("controller band: runs water when moisture is below 35, stop when it is above 80, and neither in between, after 2 reports in a row"),
        "{text}"
    );
}

#[test]
fn a_cap_of_four_a_day_enforces_a_four_in_the_purpose() {
    let purpose = "Water at most 4 times a day.";
    let text = render(&waterer(purpose, "{kind: model}", "4/day"));
    assert!(!text.contains("Nothing enforces"), "{text}");
    let text = render(&waterer(purpose, "{kind: model}", "5/day"));
    assert_eq!(
        loose(&text),
        ["4 times appears in purpose; nothing enforces it"],
        "{text}"
    );
}

#[test]
fn a_purpose_with_no_numbers_has_no_nothing_enforces_section() {
    let text = render(&mandate("Keep the shop tidy.", ""));
    assert!(!text.contains("Nothing enforces"), "{text}");
}

#[test]
fn every_field_that_matters_gets_a_sentence() {
    let m = Mandate::parse(
        "purpose: Run the farm.\nmetric: yield\n\
         controller: {kind: threshold, above: 9, action: pick, deadband: 1, hold: 3}\n\
         cadence: 1h\nnetwork: none\nrange: [0, 100]\nmax_step: 20\n\
         guards:\n  - {name: heat, adapter: shell echo 1, above: 40, then: ask}\n\
         audits:\n  - {name: weeds, adapter: shell echo 1, worse: up, tolerance: 2, window: 4}\n\
         permitted:\n  - {name: pick, run: ./pick, cap: 3/day, reversible: false, delegated: true, takes: text, cost: 7, check: ./ok, expects: up 5 within 3, revoke_after: 2}\n\
         children: [{name: a, priority: 2, floor: 5}, b]\n\
         allocation: {deadband: 3, hold: 1h}\n",
    )
    .unwrap();
    let text = render(&m);
    for want in [
        "after 3 reports in a row",
        "costs 7 a run",
        "checked by `./ok`",
        "up 5 within 3",
        "suspended after 2 misses",
        "guard heat: when heat goes above 40, it asks",
        "audit weeds: must not go up while yield goes up; tolerance 2 over 4 readings",
        "range: yield is plausible between 0 and 100",
        "max step: yield moving more than 20",
        "network none",
        "a (priority 2, floor 5), b",
        "allocation: a budget change under 3",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
}

//! Teachback: a mandate read back as plain sentences, by code and not by a
//! model, so the human sees what avsm understood before it acts. A pure
//! function from a parsed mandate to text; `Kernel::add` prints it.

use std::time::Duration;

use crate::mandate::{Controller, Mandate, Then, Worse};

/// Words after a number that are not its unit.
const NOT_UNITS: &[&str] = &[
    "and", "to", "or", "the", "a", "an", "of", "in", "on", "at", "for", "by", "is", "are", "but",
    "then", "if", "than", "this", "that", "with", "as", "from", "so", "not", "never",
];

/// A number found in a purpose.
#[derive(Clone, Debug, PartialEq)]
pub struct Figure {
    pub value: f64,
    pub percent: bool,
    /// The word after it, when that word is a unit.
    pub unit: Option<String>,
}

impl Figure {
    /// As the purpose wrote it: `15%`, `4 times`, `80`.
    pub fn text(&self) -> String {
        let mut s = num(self.value);
        if self.percent {
            s.push('%');
        }
        if let Some(u) = &self.unit {
            s.push(' ');
            s.push_str(u);
        }
        s
    }
}

/// Every integer, decimal and percentage in the text, in order.
pub fn figures(text: &str) -> Vec<Figure> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit()
            && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'));
        if !starts {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut digits = String::new();
        while j < chars.len() {
            let c = chars[j];
            let next_digit = chars.get(j + 1).is_some_and(|n| n.is_ascii_digit());
            if c.is_ascii_digit() || ((c == '.' || c == ',') && next_digit) {
                if c != ',' {
                    digits.push(c);
                }
                j += 1;
            } else {
                break;
            }
        }
        let Ok(value) = digits.parse::<f64>() else {
            i = j.max(i + 1);
            continue;
        };
        let percent = chars.get(j) == Some(&'%');
        if percent {
            j += 1;
        }
        let mut unit = None;
        if !percent {
            let rest: String = chars[j..].iter().collect();
            let word: String = rest
                .trim_start_matches(' ')
                .chars()
                .take_while(|c| c.is_alphabetic() || *c == '-')
                .collect();
            if !word.is_empty() && !NOT_UNITS.contains(&word.to_lowercase().as_str()) {
                unit = Some(word);
            }
        }
        out.push(Figure {
            value,
            percent,
            unit,
        });
        i = j;
    }
    out
}

fn num(v: f64) -> String {
    format!("{v}")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Every number the mandate holds the venture to: guard bounds and
/// drawdowns, threshold and band bounds, cap counts, costs, floors, range bounds,
/// the largest step.
pub fn enforced(m: &Mandate) -> Vec<f64> {
    let mut v = Vec::new();
    for g in &m.guards {
        v.extend(g.above);
        v.extend(g.below);
        v.extend(g.drawdown);
    }
    v.extend(m.sides().into_iter().map(|(_, bound, _)| bound));
    for p in &m.permitted {
        v.push(f64::from(p.cap.count));
        if p.cost > 0 {
            v.push(f64::from(p.cost));
        }
    }
    for c in &m.children {
        if c.floor > 0 {
            v.push(f64::from(c.floor));
        }
    }
    v.extend(m.allocation.as_ref().and_then(|a| a.total).map(f64::from));
    if let Some([lo, hi]) = m.range {
        v.push(lo);
        v.push(hi);
    }
    v.extend(m.max_step);
    v.extend(m.calibration);
    v
}

/// The numbers in the purpose that nothing in the mandate enforces.
/// Matching is numeric: `15%` matches a drawdown of 15% and a bound of 0.15.
pub fn unenforced(m: &Mandate) -> Vec<Figure> {
    let held = enforced(m);
    figures(&m.purpose)
        .into_iter()
        .filter(|f| {
            !held
                .iter()
                .any(|&e| close(f.value, e) || (f.percent && close(f.value / 100.0, e)))
        })
        .collect()
}

fn every(d: Duration) -> String {
    humantime::format_duration(d).to_string()
}

fn way(w: Worse) -> &'static str {
    match w {
        Worse::Up => "up",
        Worse::Down => "down",
    }
}

/// A controller's sentence with its hold and deadband, when set.
fn held(mut s: String, hold: Option<u32>, deadband: Option<f64>, bound: &str) -> String {
    if let Some(h) = hold.filter(|h| *h > 1) {
        s.push_str(&format!(", after {h} reports in a row"));
    }
    if let Some(d) = deadband {
        s.push_str(&format!(
            "; after acting it must come back {} past {bound} before it acts again",
            num(d)
        ));
    }
    s
}

/// The mandate as plain sentences, one line each, indented two spaces.
pub fn render(m: &Mandate) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str("  ");
        out.push_str(&s);
        out.push('\n');
    };
    let purpose = m.purpose.split_whitespace().collect::<Vec<_>>().join(" ");
    line(format!("purpose \"{purpose}\""));
    line(format!(
        "watching {} every {}{}",
        m.metric_name(),
        every(m.cadence),
        if m.metric_adapter().is_some() {
            " through the warden"
        } else {
            ""
        }
    ));
    match &m.controller {
        Controller::Threshold {
            above,
            below,
            action,
            deadband,
            hold,
        } => {
            let (side, bound) = match (above, below) {
                (Some(b), _) => ("above", num(*b)),
                (_, Some(b)) => ("below", num(*b)),
                _ => ("past", "its bound".to_string()),
            };
            let s = format!(
                "controller threshold: runs {action} when {} is {side} {bound}",
                m.metric_name()
            );
            line(held(s, *hold, *deadband, "the bound"));
        }
        Controller::Band {
            low,
            high,
            below,
            above,
            deadband,
            hold,
        } => {
            let s = format!(
                "controller band: runs {below} when {metric} is below {}, {above} when it is above {}, and neither in between",
                num(*low),
                num(*high),
                metric = m.metric_name()
            );
            line(held(s, *hold, *deadband, "its bound"));
        }
        Controller::Classifier { backend } => line(format!(
            "controller classifier: a {}backend picks nothing, a permitted action, or ask",
            backend
                .as_ref()
                .map(|b| format!("{b} "))
                .unwrap_or_default()
        )),
        Controller::Model { backend } => line(format!(
            "controller model: a {}backend proposes an action or asks",
            backend
                .as_ref()
                .map(|b| format!("{b} "))
                .unwrap_or_default()
        )),
        Controller::Plan {
            horizon,
            depth,
            members,
            explore,
            ..
        } => {
            let mut s = format!(
                "controller plan: searches up to {depth} levers over {horizon} steps through {members} members of the world model and proposes the first lever of the best sequence by its worst member; it ranks, vetoes and asks, and never authorizes"
            );
            if !explore.is_empty() {
                s.push_str(&format!("; explores {}", explore.join(", ")));
            }
            line(s);
        }
    }
    for p in &m.permitted {
        let mut s = format!(
            "may {}  {}  {}",
            p.name,
            p.cap,
            if p.reversible {
                "reversible"
            } else if p.delegated {
                "irreversible, delegated"
            } else {
                "irreversible, will ask first"
            }
        );
        match p.takes() {
            Some(t) if p.delegated => s.push_str(&format!(", takes {t}")),
            Some(t) => s.push_str(&format!(", takes {t} and will ask first")),
            None => {}
        }
        if p.cost > 0 {
            s.push_str(&format!(", costs {} a run", p.cost));
        }
        if let Some(c) = &p.check {
            s.push_str(&format!(", checked by `{c}` before it runs"));
        }
        if let Some(e) = &p.expects {
            s.push_str(&format!(", expected to move the metric {e}"));
        }
        if let Some(n) = p.revoke_after {
            s.push_str(&format!(", delegation suspended after {n} misses in a row"));
        }
        line(s);
    }
    if !m.forbidden.is_empty() {
        line(format!("never {}", m.forbidden.join(", ")));
    }
    for g in &m.guards {
        let what = match (g.drawdown, g.above, g.below) {
            (Some(d), _, _) => format!(
                "the drawdown of {} passes {}%",
                g.of.as_deref().unwrap_or_else(|| m.metric_name()),
                num(d)
            ),
            (_, Some(b), _) => format!("{} goes above {}", g.name, num(b)),
            (_, _, Some(b)) => format!("{} goes below {}", g.name, num(b)),
            _ => format!("{} has no bound", g.name),
        };
        let then = match g.then {
            Then::Pause => "it pauses the venture",
            Then::Ask => "it asks, and refuses every lever but stop until it is back inside",
        };
        let unwind = match &g.unwind {
            Some(u) => format!(
                "; first it runs {} with {} once",
                u.action,
                u.payload
                    .as_ref()
                    .map_or_else(|| "no payload".to_string(), |p| p.to_string())
            ),
            None => String::new(),
        };
        line(format!("guard {}: when {what}, {then}{unwind}", g.name));
    }
    for a in &m.audits {
        line(format!(
            "audit {}: must not go {} while {} goes {}; tolerance {} over {} readings",
            a.name,
            way(a.worse),
            m.metric_name(),
            way(a.better),
            num(a.tolerance),
            a.window
        ));
    }
    if let Some([lo, hi]) = m.range {
        line(format!(
            "range: {} is plausible between {} and {}",
            m.metric_name(),
            num(lo),
            num(hi)
        ));
    }
    if let Some(s) = m.max_step {
        line(format!(
            "max step: {} moving more than {} between reports is implausible",
            m.metric_name(),
            num(s)
        ));
        if !m.confirm {
            line(format!(
                "confirm: off; a report of {} right after an implausible one is acted on at once",
                m.metric_name()
            ));
        }
    }
    if let Some(w) = &m.world {
        line(format!(
            "world model: {w} forecasts {}; it ranks, vetoes and asks, and never authorizes",
            m.metric_name()
        ));
    }
    match m.direction() {
        Some(w) => line(format!(
            "better: {} going {} is the target improving; capacity is checked against it",
            m.metric_name(),
            way(w)
        )),
        None => line("no direction declared; capacity is not checked".to_string()),
    }
    if let Some(c) = m.calibration {
        line(format!(
            "calibration: the world model's rankings run only once its mean skill over the last 20 scores is at least {}; until then they ask",
            num(c)
        ));
    }
    match &m.network {
        None => {}
        Some(h) if h.is_empty() => line("network none".to_string()),
        Some(h) => line(format!("network only {}", h.join(", "))),
    }
    if !m.children.is_empty() {
        line(format!(
            "parent of {}",
            m.children
                .iter()
                .map(|c| {
                    if c.priority == 0 && c.floor == 0 {
                        c.name.clone()
                    } else {
                        format!("{} (priority {}, floor {})", c.name, c.priority, c.floor)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(a) = &m.allocation {
        line(format!(
            "allocation: a budget change under {} is refused; a reversal inside {} is refused",
            a.deadband,
            every(a.hold)
        ));
        if let Some(t) = a.total {
            line(format!(
                "allocation: a budget that brings the children's budgets past {t} is refused"
            ));
        }
    }
    if let Some(p) = &m.parent {
        line(format!("child of {p}"));
    }
    let loose = unenforced(m);
    if !loose.is_empty() {
        line("Nothing enforces:".to_string());
        for f in loose {
            line(format!(
                "  {} appears in purpose; nothing enforces it",
                f.text()
            ));
        }
    }
    out
}

//! What a run was expected to do to the metric, against what the next
//! reports say it did, net of what the metric would have done anyway.
//! Pure arithmetic on trusted reports: no model, no ledger. The kernel
//! supplies the counterfactual drift and its spread, and writes what
//! follows from the answer.

use crate::mandate::{Direction, Expects};

/// How many reports before a run set the noise.
const NOISE_WINDOW: usize = 20;

/// The fewest steps between reports that make a noise estimate.
const NOISE_STEPS: usize = 3;

/// What a run did to the metric, judged against what the mandate expects.
#[derive(Clone, Debug, PartialEq)]
pub enum Judgement {
    /// Fewer than `within` reports since the run's ok outcome.
    Waiting,
    /// No report before the run to compare with.
    Unknown,
    /// As expected, within tolerance.
    Met,
    /// The right way, not by the size expected. `observed` is the change;
    /// `effect` the change net of drift.
    WrongSize {
        observed: f64,
        effect: f64,
        noise: f64,
    },
    /// Moved, net of drift, no more than the metric strays on its own.
    NoEffect {
        observed: f64,
        effect: f64,
        noise: f64,
    },
    /// Moved, net of drift, the other way by more than the noise.
    WrongDirection {
        observed: f64,
        effect: f64,
        noise: f64,
    },
}

/// The median size of a step between consecutive reports, over the last
/// 20 of `before`; 0 when there are fewer than 3 steps.
pub fn noise(before: &[f64]) -> f64 {
    let from = before.len().saturating_sub(NOISE_WINDOW);
    let mut steps: Vec<f64> = before[from..]
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .collect();
    if steps.len() < NOISE_STEPS {
        return 0.0;
    }
    steps.sort_by(f64::total_cmp);
    let mid = steps.len() / 2;
    if steps.len() % 2 == 1 {
        steps[mid]
    } else {
        (steps[mid - 1] + steps[mid]) / 2.0
    }
}

/// The significance level a span of exact repeats after a run is judged
/// at, the stuck test's (`kernel::ALPHA`).
pub use crate::kernel::ALPHA;

/// How many of the latest steps before a run say how often the metric
/// repeats exactly, the stuck test's (`kernel::WINDOW`).
pub use crate::kernel::WINDOW;

/// The fewest steps a history of repeats is read from, as for the drift
/// (`world::MIN_SAMPLES`).
pub use crate::world::MIN_SAMPLES;

/// How often the metric's readings repeat exactly: of the latest WINDOW
/// steps between consecutive readings of `before` (oldest first), how
/// many there are and how many are exact repeats. A step a run of the
/// lever fell in (`ran` says, for each step, whether one did) is what the
/// lever did, not the sensor, and a repeat at a bound of the range
/// (`bound`) says nothing of the sensor: neither is a step.
pub fn repeats(before: &[f64], ran: &[bool], bound: impl Fn(f64) -> bool) -> (usize, usize) {
    let from = before.len().saturating_sub(WINDOW + 1);
    let (mut steps, mut zero) = (0, 0);
    for (i, w) in before.windows(2).enumerate().skip(from) {
        if ran.get(i).copied().unwrap_or(false) {
            continue;
        }
        if w[1] == w[0] {
            if bound(w[0]) {
                continue;
            }
            zero += 1;
        }
        steps += 1;
    }
    (steps, zero)
}

/// The chance that a sensor holds a reading `r` more times in a row, after
/// `zero` exact repeats in `steps` steps of its history, under no prior
/// belief about how often it repeats (uniform): the posterior predictive,
/// prod_{i<r} (zero+1+i)/(steps+2+i), as the stuck test reads it. Each
/// repeat is evidence for the next, so a thin history never makes a hold
/// look rarer than it can say. It stops once below ALPHA, the only
/// question asked of it.
pub fn hold_chance(steps: usize, zero: usize, r: usize) -> f64 {
    let mut chance = 1.0;
    for i in 0..r {
        chance *= (zero + 1 + i) as f64 / (steps + 2 + i) as f64;
        if chance < ALPHA {
            break;
        }
    }
    chance
}

/// When a run is judged, as far as a span of exact repeats goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Horizon {
    /// At this many plausible reports after the run.
    At(usize),
    /// Every report since repeats the last before the run, and a sensor
    /// that has not refreshed shows a span that long at least as often as
    /// ALPHA: waiting on a reading that differs.
    Waiting,
    /// Every report through the declared horizon repeats the last before
    /// the run, and the history cannot say how often the sensor holds a
    /// reading: fewer than MIN_SAMPLES steps, or none that changed. No
    /// evidence either way; not judged.
    Thin,
}

/// When a run is judged (`before` the plausible reports before its intent,
/// oldest first; `ran` whether the lever ran in each step between them;
/// `after` the plausible reports since its ok outcome; `since` how many
/// trusted ones came, plausible or not; `bound` whether a value is a
/// bound of the range): at `within`, unless every one of those exactly
/// repeats the last report before the run. Such a span holds no fresh
/// reading. It is evidence of no effect only once a sensor that repeats
/// as often as the metric's history says would have refreshed: once the
/// chance of that many repeats in a row (`hold_chance`) is below ALPHA.
/// Before that the run waits, and the first reading that differs judges
/// it; past it, the miss at `within` stands. A history too thin to say
/// how often the sensor repeats judges nothing (`Thin`). Readings flagged
/// implausible count toward the wait, so no sensor holds a lever for ever.
pub fn horizon(
    within: usize,
    before: &[f64],
    ran: &[bool],
    after: &[f64],
    since: usize,
    bound: impl Fn(f64) -> bool,
) -> Horizon {
    let Some(&base) = before.last() else {
        return Horizon::At(within);
    };
    let same = after.iter().take_while(|x| **x == base).count();
    if same < within {
        return Horizon::At(within);
    }
    let (steps, zero) = repeats(before, ran, bound);
    if steps < MIN_SAMPLES || zero >= steps {
        return Horizon::Thin;
    }
    let rare = |r: usize| hold_chance(steps, zero, r) < ALPHA;
    if rare(same) {
        return Horizon::At(within);
    }
    if same < after.len() {
        return Horizon::At(same + 1);
    }
    if rare(since) {
        Horizon::At(within)
    } else {
        Horizon::Waiting
    }
}

/// Judge one run. `before` is the trusted reports of the metric before
/// the run's intent, oldest first; `after` those since its ok outcome.
/// The change is the `within`-th report after, minus the last before.
/// `drift` is what the metric was expected to move over that interval on
/// its own, and `noise` how far it strays from that; the run is judged on
/// its effect, the change net of drift.
pub fn judge(
    expects: &Expects,
    before: &[f64],
    after: &[f64],
    drift: f64,
    noise: f64,
) -> Judgement {
    let Some(&base) = before.last() else {
        return Judgement::Unknown;
    };
    let Some(&then) = after.get(expects.within as usize - 1) else {
        return Judgement::Waiting;
    };
    judge_change(expects, then - base, drift, noise)
}

/// Judge a change the metric made over a span a run is judged on, net of
/// the `drift` over it, against the `noise`.
pub fn judge_change(expects: &Expects, observed: f64, drift: f64, noise: f64) -> Judgement {
    let effect = observed - drift;
    if effect.abs() <= noise {
        return Judgement::NoEffect {
            observed,
            effect,
            noise,
        };
    }
    let toward = toward(expects, effect);
    if toward < 0.0 {
        return Judgement::WrongDirection {
            observed,
            effect,
            noise,
        };
    }
    match expects.size {
        Some(size) if (toward - size).abs() > (size / 2.0).max(noise) => Judgement::WrongSize {
            observed,
            effect,
            noise,
        },
        _ => Judgement::Met,
    }
}

/// The standard deviation of a step for each unit of its median size,
/// when the steps are normal about nothing (1 / 0.6745).
const SPREAD: f64 = 1.4826;

/// The point `z` such that a standard normal exceeds it with chance `p`
/// (Abramowitz and Stegun 26.2.23, for 0 < p < 1/2).
fn upper_point(p: f64) -> f64 {
    let t = (-2.0 * p.ln()).sqrt();
    t - (2.515517 + 0.802853 * t + 0.010328 * t * t)
        / (1.0 + 1.432788 * t + 0.189269 * t * t + 0.001308 * t * t * t)
}

/// What many runs together show of a lever that no run showed alone.
#[derive(Clone, Debug, PartialEq)]
pub struct Pooled {
    /// The mean effect the right way, net of drift, over the runs.
    pub mean: f64,
    /// The noise the runs were judged against, pooled.
    pub noise: f64,
    /// The most the lever's effect can be, at the significance ALPHA:
    /// the mean and the band the mean of that many runs strays by.
    pub bound: f64,
    pub runs: usize,
}

/// Do the runs together show no effect? `runs` holds, for each judged run
/// since the lever was last restored, its effect the right way (net of
/// drift) and the noise it was judged against. One run is read against
/// the noise, and a metric that strays by the noise on its own makes a
/// run that "meets" by chance as often as one that misses, so misses in
/// a row never come. The mean of k runs strays by 1/sqrt(k) of what one
/// does. The runs show no effect when the most their mean can be, at the
/// significance ALPHA (one-sided), is still within the noise: the least
/// a single run counts as an effect by. That takes about 12 runs when
/// the lever does nothing and the noise is all there is, and never
/// happens while the mean is a noise or more. Nothing is shown when the
/// noise is not known.
pub fn pooled(runs: &[(f64, f64)]) -> Option<Pooled> {
    let k = runs.len();
    if k == 0 {
        return None;
    }
    let mean = runs.iter().map(|r| r.0).sum::<f64>() / k as f64;
    let noise = (runs.iter().map(|r| r.1 * r.1).sum::<f64>() / k as f64).sqrt();
    if noise <= 0.0 {
        return None;
    }
    let bound = mean + upper_point(ALPHA) * SPREAD * noise / (k as f64).sqrt();
    (bound < noise).then_some(Pooled {
        mean,
        noise,
        bound,
        runs: k,
    })
}

/// An effect read the way the lever expects it: positive is the right way.
pub fn toward(expects: &Expects, effect: f64) -> f64 {
    match expects.direction {
        Direction::Up => effect,
        Direction::Down => -effect,
    }
}

/// The delay a lever's effect shows at: the shift, in reports, at which
/// the metric's response best lines up with its runs. `runs` holds, for
/// each run, its effect the right way (net of drift) and the noise, over
/// `within` reports starting at each shift: the 0th from the last report
/// before the run, the `d`-th from the `d`-th after it. Only the shifts
/// every run has count. The response at a shift is the runs' effects
/// summed over their noise pooled (a z-score), so a shift that lines up
/// in every run beats one that a single run pulls; ties go to the
/// earlier shift, and with no runs it is 0, the declared horizon.
pub fn delay(runs: &[Vec<(f64, f64)>]) -> usize {
    let shifts = runs.iter().map(Vec::len).min().unwrap_or(0);
    let mut best = (0, f64::NEG_INFINITY);
    for d in 0..shifts {
        let effect: f64 = runs.iter().map(|r| r[d].0).sum();
        let noise = runs.iter().map(|r| r[d].1 * r[d].1).sum::<f64>().sqrt();
        // With no noise at all, any effect the right way is certain.
        let score = if noise > 0.0 {
            effect / noise
        } else if effect == 0.0 {
            0.0
        } else {
            effect.signum() * f64::INFINITY
        };
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Judged with no drift and the median step as the noise.
    fn still(e: &Expects, before: &[f64], after: &[f64]) -> Judgement {
        judge(e, before, after, 0.0, noise(before))
    }

    #[test]
    fn a_run_is_judged_net_of_the_drift() {
        // Evaporation pulls 10 a step; water shows +4.5, so it added 14.5.
        let e = up(None, 1);
        assert_eq!(
            judge(&e, &[60.0, 50.0, 40.0], &[44.5], -10.0, 0.0),
            Judgement::Met
        );
        // On the raw change, the same run is no effect against noise 10.
        assert!(matches!(
            judge(&e, &[60.0, 50.0, 40.0], &[44.5], 0.0, 10.0),
            Judgement::NoEffect { .. }
        ));
        // A dead plant: no drift, no change.
        assert!(matches!(
            judge(&e, &[0.0, 0.0, 0.0], &[0.0], 0.0, 0.0),
            Judgement::NoEffect { effect, .. } if effect == 0.0
        ));
        // Falling slower than the drift is the right way.
        let down = Expects {
            direction: Direction::Down,
            size: None,
            within: 1,
        };
        assert!(matches!(
            judge(&down, &[10.0], &[8.0], -5.0, 0.5),
            Judgement::WrongDirection { effect, .. } if effect == 3.0
        ));
    }

    fn up(size: Option<f64>, within: u32) -> Expects {
        Expects {
            direction: Direction::Up,
            size,
            within,
        }
    }

    #[test]
    fn the_delay_is_where_the_runs_line_up_best() {
        assert_eq!(delay(&[]), 0, "no runs: the declared horizon");
        let late = vec![(0.0, 1.0), (0.5, 1.0), (6.0, 1.0), (1.0, 1.0)];
        assert_eq!(delay(std::slice::from_ref(&late)), 2);
        // A shift every run shows beats one a single run pulls.
        let pulled = vec![(0.0, 1.0), (3.0, 1.0), (10.0, 1.0)];
        let steady = vec![(0.0, 1.0), (3.0, 1.0), (-6.0, 1.0)];
        assert_eq!(delay(&[pulled, steady]), 1);
        // Only the shifts every run has whole count.
        assert_eq!(delay(&[late, vec![(1.0, 1.0), (0.0, 1.0)]]), 0);
        // Ties go to the earlier shift; with no noise any effect is sure.
        assert_eq!(delay(&[vec![(2.0, 1.0), (2.0, 1.0)]]), 0);
        assert_eq!(delay(&[vec![(0.0, 0.0), (0.1, 0.0)]]), 1);
        assert_eq!(delay(&[vec![(0.0, 0.0), (-0.1, 0.0)]]), 0);
        // Noise pools as independent strays do: 6 over √2 beats 4 over
        // √1.45, though 6 over 2 would not beat 4 over 1.3.
        let a = vec![(3.0, 1.0), (4.0, 1.2)];
        let b = vec![(3.0, 1.0), (0.0, 0.1)];
        assert_eq!(delay(&[a, b]), 0);
    }

    /// A sensor read five times a refresh: four exact repeats, then a
    /// change, `n` times over, from 60 up by 10.
    fn lagging(n: usize) -> Vec<f64> {
        (0..n)
            .flat_map(|k| std::iter::repeat_n(60.0 + 10.0 * k as f64, 5))
            .collect()
    }

    fn unbounded(_: f64) -> bool {
        false
    }

    #[test]
    fn repeats_count_the_steps_the_lever_did_not_run_in() {
        assert_eq!(repeats(&[], &[], unbounded), (0, 0));
        assert_eq!(repeats(&[3.0], &[], unbounded), (0, 0));
        assert_eq!(repeats(&lagging(3), &[], unbounded), (14, 12));
        // A step a run fell in is the lever's, whatever it read.
        let mut ran = vec![false; 14];
        ran[4] = true;
        ran[6] = true;
        assert_eq!(repeats(&lagging(3), &ran, unbounded), (12, 11));
        // A repeat at a bound is no step of the sensor.
        let at = |x: f64| x == 70.0;
        assert_eq!(repeats(&lagging(3), &[], at), (10, 8));
        // Only the latest WINDOW steps count.
        let mut long = vec![1.0; 1000];
        long.extend((0..WINDOW).map(|i| i as f64 + 2.0));
        assert_eq!(repeats(&long, &[], unbounded), (WINDOW, 0));
    }

    #[test]
    fn the_hold_chance_is_the_posterior_predictive() {
        assert_eq!(hold_chance(10, 3, 0), 1.0);
        // No history: 1/2, then 1/2 * 2/3.
        assert!((hold_chance(0, 0, 2) - 1.0 / 3.0).abs() < 1e-12);
        // 3 repeats in 10: 4/12 * 5/13.
        assert!((hold_chance(10, 3, 2) - 4.0 / 12.0 * 5.0 / 13.0).abs() < 1e-12);
        // 20 steps that never repeated: one repeat is 1/22, two 1/22 * 2/23.
        assert!(hold_chance(20, 0, 1) > ALPHA);
        assert!(hold_chance(20, 0, 2) < ALPHA);
    }

    #[test]
    fn a_span_of_repeats_waits_while_the_sensor_may_still_hold() {
        // Three refreshes, four repeats each: a hold of 4 is likely.
        let before = lagging(4);
        let base = *before.last().unwrap();
        let same = |n: usize| vec![base; n];
        let h =
            |after: &[f64], since: usize| horizon(3, &before[..16], &[], after, since, unbounded);
        // A change inside the declared horizon: judged as declared.
        assert_eq!(h(&[90.0, 75.0], 2), Horizon::At(3));
        assert_eq!(h(&[90.0, 75.0, 75.0], 3), Horizon::At(3));
        // Unchanged through it: waits, and the first change judges.
        assert_eq!(h(&same(3), 3), Horizon::Waiting);
        assert_eq!(h(&same(4), 4), Horizon::Waiting);
        assert_eq!(h(&[90.0, 90.0, 90.0, 90.0, 75.0], 5), Horizon::At(5));
        // Held so long that a sensor this one would have refreshed: the
        // miss at the declared horizon stands.
        let r = (3..).find(|r| hold_chance(15, 12, *r) < ALPHA).unwrap();
        assert_eq!(h(&same(r - 1), r - 1), Horizon::Waiting);
        assert_eq!(h(&same(r), r), Horizon::At(3));
        let mut late = same(r);
        late.push(75.0);
        assert_eq!(h(&late, r + 1), Horizon::At(3));
        // Readings flagged implausible count toward the wait.
        assert_eq!(h(&same(3), r), Horizon::At(3));
    }

    #[test]
    fn a_history_that_cannot_say_how_long_the_sensor_holds_judges_nothing() {
        // No history, a step or two, or one that never changed: no
        // evidence either way.
        assert_eq!(
            horizon(2, &[], &[], &[3.0, 3.0], 2, unbounded),
            Horizon::At(2)
        );
        assert_eq!(
            horizon(2, &[3.0], &[], &[3.0, 3.0], 2, unbounded),
            Horizon::Thin
        );
        assert_eq!(
            horizon(2, &[1.0, 2.0, 3.0], &[], &[3.0, 3.0], 2, unbounded),
            Horizon::Thin
        );
        assert_eq!(
            horizon(2, &[3.0; 9], &[], &[3.0, 3.0], 2, unbounded),
            Horizon::Thin
        );
        // A change inside the horizon is judged whatever the history.
        assert_eq!(
            horizon(2, &[3.0], &[], &[3.0, 4.0], 2, unbounded),
            Horizon::At(2)
        );
        // A metric that never repeated in 20 steps: two repeats are rare
        // enough to judge, one is not.
        let varied: Vec<f64> = (0..21).map(f64::from).collect();
        assert_eq!(
            horizon(2, &varied, &[], &[20.0, 20.0], 2, unbounded),
            Horizon::At(2)
        );
        assert_eq!(
            horizon(1, &varied, &[], &[20.0], 1, unbounded),
            Horizon::Waiting
        );
        assert_eq!(
            horizon(1, &varied, &[], &[20.0, 20.0], 2, unbounded),
            Horizon::At(1)
        );
        // Steps the lever ran in say nothing of the sensor.
        let ran = vec![true; 20];
        assert_eq!(
            horizon(2, &varied, &ran, &[20.0, 20.0], 2, unbounded),
            Horizon::Thin
        );
    }

    #[test]
    fn many_runs_inside_the_noise_show_no_effect_where_one_does_not() {
        assert!((upper_point(0.01) - 2.326).abs() < 0.01);
        let runs = |effect: f64, n: usize| vec![(effect, 1.0); n];
        // A run of nothing: about 12 of them say so, 3 do not.
        assert_eq!(pooled(&[]), None);
        assert_eq!(pooled(&runs(0.0, 3)), None);
        let found = (1..30).find(|n| pooled(&runs(0.0, *n)).is_some()).unwrap();
        assert!((11..=13).contains(&found), "{found}");
        // A mean of a noise or more never does, however many runs.
        assert_eq!(pooled(&runs(1.0, 1000)), None);
        assert_eq!(pooled(&runs(3.0, 1000)), None);
        // Half a noise takes longer; the wrong way sooner.
        let half = (1..400).find(|n| pooled(&runs(0.5, *n)).is_some()).unwrap();
        assert!(half > 3 * found, "{half}");
        let wrong = (1..30).find(|n| pooled(&runs(-1.0, *n)).is_some()).unwrap();
        assert!(wrong < found, "{wrong}");
        // No noise known says nothing.
        assert_eq!(pooled(&[(0.0, 0.0); 50]), None);
        // The noise is pooled as independent strays are.
        let p = pooled(&[(0.0, 3.0), (0.0, 4.0)].repeat(10)).unwrap();
        assert!((p.noise - (12.5f64).sqrt()).abs() < 1e-12 && p.runs == 20);
    }

    #[test]
    fn noise_is_zero_under_three_steps() {
        assert_eq!(noise(&[1.0, 5.0, 2.0]), 0.0);
    }

    #[test]
    fn noise_is_the_median_step() {
        // steps 1, 3, 2, 10: the median of 1 2 3 10 is 2.5
        assert_eq!(noise(&[0.0, 1.0, 4.0, 2.0, 12.0]), 2.5);
        // steps 1, 3, 2
        assert_eq!(noise(&[0.0, 1.0, 4.0, 2.0]), 2.0);
    }

    #[test]
    fn noise_looks_at_the_last_twenty() {
        let mut before = vec![0.0, 100.0, 0.0, 100.0, 0.0];
        before.extend((0..20).map(f64::from));
        assert_eq!(noise(&before), 1.0);
    }

    #[test]
    fn waits_for_the_nth_report() {
        assert_eq!(still(&up(None, 3), &[1.0], &[5.0, 6.0]), Judgement::Waiting);
        assert_eq!(
            still(&up(None, 3), &[1.0], &[5.0, 6.0, 7.0]),
            Judgement::Met
        );
    }

    #[test]
    fn the_nth_report_is_the_one_that_counts() {
        assert_eq!(
            still(&up(None, 2), &[1.0], &[1.0, 9.0, 0.0]),
            Judgement::Met
        );
    }

    #[test]
    fn no_baseline_is_unknown() {
        assert_eq!(still(&up(None, 1), &[], &[9.0]), Judgement::Unknown);
    }

    #[test]
    fn down_reads_the_sign_the_other_way() {
        let down = Expects {
            direction: Direction::Down,
            size: Some(4.0),
            within: 1,
        };
        assert_eq!(still(&down, &[10.0], &[6.0]), Judgement::Met);
        assert!(matches!(
            still(&down, &[10.0], &[14.0]),
            Judgement::WrongDirection { .. }
        ));
    }

    #[test]
    fn size_tolerance_is_half_the_size_or_the_noise() {
        let e = up(Some(4.0), 1);
        assert_eq!(still(&e, &[0.0], &[6.0]), Judgement::Met);
        assert_eq!(still(&e, &[0.0], &[2.0]), Judgement::Met);
        assert!(matches!(
            still(&e, &[0.0], &[6.5]),
            Judgement::WrongSize { observed, .. } if observed == 6.5
        ));
        // Noisy enough that 4 off is inside the noise: steps of 4 or more.
        let noisy = [0.0, 4.0, 0.0, 4.0, 0.0];
        assert_eq!(still(&e, &noisy, &[8.0]), Judgement::Met);
    }

    #[test]
    fn a_move_inside_the_noise_is_no_effect() {
        let noisy = [0.0, 4.0, 0.0, 4.0, 0.0];
        assert!(matches!(
            still(&up(None, 1), &noisy, &[4.0]),
            Judgement::NoEffect { .. }
        ));
        assert!(matches!(
            still(&up(None, 1), &noisy, &[-4.0]),
            Judgement::NoEffect { .. }
        ));
    }
}

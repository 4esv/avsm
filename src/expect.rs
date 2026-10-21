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

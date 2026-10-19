//! What a run was expected to do to the metric, against what the next
//! reports say it did. Pure arithmetic on trusted reports: no model, no
//! ledger. The kernel writes what follows from the answer.

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
    /// The right way, not by the size expected. `observed` is the change.
    WrongSize { observed: f64, noise: f64 },
    /// Moved no more than the metric moves on its own.
    NoEffect { observed: f64, noise: f64 },
    /// Moved the other way by more than the metric moves on its own.
    WrongDirection { observed: f64, noise: f64 },
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
pub fn judge(expects: &Expects, before: &[f64], after: &[f64]) -> Judgement {
    let Some(&base) = before.last() else {
        return Judgement::Unknown;
    };
    let Some(&then) = after.get(expects.within as usize - 1) else {
        return Judgement::Waiting;
    };
    let observed = then - base;
    let noise = noise(before);
    if observed.abs() <= noise {
        return Judgement::NoEffect { observed, noise };
    }
    let toward = match expects.direction {
        Direction::Up => observed,
        Direction::Down => -observed,
    };
    if toward < 0.0 {
        return Judgement::WrongDirection { observed, noise };
    }
    match expects.size {
        Some(size) if (toward - size).abs() > (size / 2.0).max(noise) => {
            Judgement::WrongSize { observed, noise }
        }
        _ => Judgement::Met,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(size: Option<f64>, within: u32) -> Expects {
        Expects {
            direction: Direction::Up,
            size,
            within,
        }
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
        assert_eq!(judge(&up(None, 3), &[1.0], &[5.0, 6.0]), Judgement::Waiting);
        assert_eq!(
            judge(&up(None, 3), &[1.0], &[5.0, 6.0, 7.0]),
            Judgement::Met
        );
    }

    #[test]
    fn the_nth_report_is_the_one_that_counts() {
        assert_eq!(
            judge(&up(None, 2), &[1.0], &[1.0, 9.0, 0.0]),
            Judgement::Met
        );
    }

    #[test]
    fn no_baseline_is_unknown() {
        assert_eq!(judge(&up(None, 1), &[], &[9.0]), Judgement::Unknown);
    }

    #[test]
    fn down_reads_the_sign_the_other_way() {
        let down = Expects {
            direction: Direction::Down,
            size: Some(4.0),
            within: 1,
        };
        assert_eq!(judge(&down, &[10.0], &[6.0]), Judgement::Met);
        assert!(matches!(
            judge(&down, &[10.0], &[14.0]),
            Judgement::WrongDirection { .. }
        ));
    }

    #[test]
    fn size_tolerance_is_half_the_size_or_the_noise() {
        let e = up(Some(4.0), 1);
        assert_eq!(judge(&e, &[0.0], &[6.0]), Judgement::Met);
        assert_eq!(judge(&e, &[0.0], &[2.0]), Judgement::Met);
        assert!(matches!(
            judge(&e, &[0.0], &[6.5]),
            Judgement::WrongSize { observed, .. } if observed == 6.5
        ));
        // Noisy enough that 4 off is inside the noise: steps of 4 or more.
        let noisy = [0.0, 4.0, 0.0, 4.0, 0.0];
        assert_eq!(judge(&e, &noisy, &[8.0]), Judgement::Met);
    }

    #[test]
    fn a_move_inside_the_noise_is_no_effect() {
        let noisy = [0.0, 4.0, 0.0, 4.0, 0.0];
        assert!(matches!(
            judge(&up(None, 1), &noisy, &[4.0]),
            Judgement::NoEffect { .. }
        ));
        assert!(matches!(
            judge(&up(None, 1), &noisy, &[-4.0]),
            Judgement::NoEffect { .. }
        ));
    }
}

//! Critical slowing down: a system nearing a tipping point recovers more
//! slowly from small shocks, so its readings wander further and remember
//! more of the step before. Over the last `SPAN` plausible reports, compare
//! the variance and the lag-1 autocorrelation of the first `W` with the
//! last `W`. Both rising past their margins is a warning. The first warning
//! after `QUIET` reports without one starts an episode, and only that
//! report asks. Pure: numbers in, a verdict out.
//!
//! The parameters are calibrated, not guessed: see
//! `false_alarms_stay_under_two_percent_and_the_ramp_is_caught`, which
//! bounds the false-alarm rate on white noise and the hit rate on an AR(1)
//! whose coefficient ramps 0.2 to 0.95 with shocks growing 1 to 3, over
//! 1000 seeded streams of 60 reports each.

/// How many reports a comparison spans.
pub const SPAN: usize = 55;
/// The reports compared at each end of the span.
pub const W: usize = 20;
/// The last `W`'s variance must exceed the first `W`'s by this factor.
pub const VARIANCE_RATIO: f64 = 3.0;
/// The last `W`'s lag-1 autocorrelation must exceed the first's by this.
pub const AUTOCORRELATION_RISE: f64 = 0.1;
/// Reports without a warning before a warning starts a new episode: an
/// episode cannot flicker.
pub const QUIET: usize = W;

/// The variance of a sample about its mean, and its lag-1 autocorrelation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub variance: f64,
    pub autocorrelation: f64,
}

/// The stats of a sample. None for fewer than three values.
pub fn stats(values: &[f64]) -> Option<Stats> {
    let n = values.len();
    if n < 3 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let dev: Vec<f64> = values.iter().map(|x| x - mean).collect();
    let ss: f64 = dev.iter().map(|d| d * d).sum();
    let variance = ss / n as f64;
    let autocorrelation = if ss > 0.0 {
        dev.windows(2).map(|w| w[0] * w[1]).sum::<f64>() / ss
    } else {
        0.0
    };
    Some(Stats {
        variance,
        autocorrelation,
    })
}

/// What a warning says: how the two ends of the span compare.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rise {
    pub first: Stats,
    pub second: Stats,
}

impl Rise {
    pub fn variance_ratio(&self) -> f64 {
        if self.first.variance > 0.0 {
            self.second.variance / self.first.variance
        } else if self.second.variance > 0.0 {
            f64::INFINITY
        } else {
            1.0
        }
    }

    pub fn autocorrelation_rise(&self) -> f64 {
        self.second.autocorrelation - self.first.autocorrelation
    }

    /// Both rising past their margins.
    pub fn warns(&self) -> bool {
        self.variance_ratio() > VARIANCE_RATIO && self.autocorrelation_rise() > AUTOCORRELATION_RISE
    }
}

/// The first and last `W` of the last `SPAN` values, oldest first. None
/// with fewer.
pub fn compare(series: &[f64]) -> Option<Rise> {
    if series.len() < SPAN {
        return None;
    }
    let last = &series[series.len() - SPAN..];
    Some(Rise {
        first: stats(&last[..W])?,
        second: stats(&last[SPAN - W..])?,
    })
}

/// Does the series, as it stood after its first `n` values, warn?
fn warns_at(series: &[f64], n: usize) -> bool {
    compare(&series[..n]).is_some_and(|r| r.warns())
}

/// Does the latest value start an episode: a warning now, and none in the
/// `QUIET` values before? Returns what warns.
pub fn starts(series: &[f64]) -> Option<Rise> {
    let now = compare(series).filter(Rise::warns)?;
    let n = series.len();
    let before = (n.saturating_sub(QUIET)..n).any(|k| warns_at(series, k));
    (!before).then_some(now)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::world::splitmix64;

    /// A standard normal draw, Box-Muller over splitmix64.
    pub fn normal(rng: &mut u64) -> f64 {
        let u = |rng: &mut u64| ((splitmix64(rng) >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let (a, b) = (u(rng), u(rng));
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }

    /// An AR(1) series whose coefficient ramps from 0.2 to 0.95 and whose
    /// shocks grow from 1 to 3 over `n` steps.
    pub fn ramp(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = seed;
        let mut x = 0.0;
        (0..n)
            .map(|i| {
                let t = i as f64 / (n - 1) as f64;
                let phi = 0.2 + 0.75 * t;
                let sigma = 1.0 + 2.0 * t;
                x = phi * x + sigma * normal(&mut rng);
                100.0 + x
            })
            .collect()
    }

    pub fn white(n: usize, seed: u64) -> Vec<f64> {
        let mut rng = seed;
        (0..n).map(|_| 100.0 + normal(&mut rng)).collect()
    }

    /// How many episodes start as the series arrives, one value at a time.
    fn episodes(series: &[f64]) -> usize {
        (1..=series.len())
            .filter(|&n| starts(&series[..n]).is_some())
            .count()
    }

    /// The streams the rates are measured over: 60 reports each.
    const STREAM: usize = 60;
    const SEEDS: u64 = 1000;

    #[test]
    fn stats_of_known_values() {
        let s = stats(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert!((s.variance - 1.25).abs() < 1e-12);
        // Deviations -1.5 -.5 .5 1.5: (.75 - .25 + .75) / 5 = .25.
        assert!((s.autocorrelation - 0.25).abs() < 1e-12, "{s:?}");
        assert_eq!(stats(&[5.0; 4]).unwrap().autocorrelation, 0.0);
        assert!(stats(&[1.0, 2.0]).is_none());
        assert!(compare(&[1.0; SPAN - 1]).is_none());
    }

    /// The calibration: over 1000 seeds of 60 reports, white noise warns in
    /// at most 2% of streams, the ramp in at least 80%.
    #[test]
    fn false_alarms_stay_under_two_percent_and_the_ramp_is_caught() {
        let warned = |f: fn(usize, u64) -> Vec<f64>, offset: u64| {
            (0..SEEDS)
                .filter(|s| episodes(&f(STREAM, offset + s)) > 0)
                .count()
        };
        let ramp_rate = warned(ramp, 0) as f64 / SEEDS as f64;
        let white_rate = warned(white, 1_000_000) as f64 / SEEDS as f64;
        println!("ramp warned in {ramp_rate:.3}, white noise in {white_rate:.3}");
        assert!(white_rate <= 0.02, "white noise warned in {white_rate}");
        assert!(ramp_rate >= 0.80, "the ramp warned in {ramp_rate}");
    }

    #[test]
    fn a_ramping_ar1_warns_once_and_white_noise_does_not() {
        let series = ramp(STREAM, 2);
        let last = compare(&series).unwrap();
        assert_eq!(
            episodes(&series),
            1,
            "variance x{:.2}, autocorrelation {:+.2}",
            last.variance_ratio(),
            last.autocorrelation_rise()
        );
        assert_eq!(episodes(&white(STREAM, 2)), 0);
    }

    /// An episode is a run of warning reports with gaps shorter than
    /// `QUIET`; only its first report starts it.
    #[test]
    fn an_episode_starts_only_after_quiet_and_cannot_flicker() {
        // A ramp whose warning comes and goes.
        let flickers = |s: &Vec<f64>| {
            let w: Vec<bool> = (1..=s.len()).map(|n| warns_at(s, n)).collect();
            w.windows(3).any(|x| x[0] && !x[1] && x[2])
        };
        let long = (0..500u64)
            .map(|seed| ramp(STREAM + 20, seed))
            .find(flickers)
            .expect("a ramp whose warning comes and goes");
        for n in 1..=long.len() {
            let quiet = (n.saturating_sub(QUIET)..n).all(|k| !warns_at(&long, k));
            assert_eq!(
                starts(&long[..n]).is_some(),
                warns_at(&long, n) && quiet,
                "report {n}"
            );
        }
        // The warning that came back did not start a second episode.
        let rises = (1..=long.len())
            .filter(|&n| warns_at(&long, n) && !warns_at(&long, n - 1))
            .count();
        assert!(rises >= 2, "it flickered");
        assert!(episodes(&long) < rises, "one episode, not one per flicker");
    }

    /// A span with `first` at its start, `last` at its end, a still middle.
    fn span(first: &[f64], last: &[f64]) -> Vec<f64> {
        let mut s = first.to_vec();
        s.extend(std::iter::repeat_n(0.0, SPAN - 2 * W));
        s.extend_from_slice(last);
        s
    }

    #[test]
    fn rising_variance_alone_or_rising_autocorrelation_alone_does_not_warn() {
        // Variance up ninefold, both ends white: no memory gained.
        let mut rng = 3;
        let quiet: Vec<f64> = (0..W).map(|_| normal(&mut rng)).collect();
        let loud: Vec<f64> = (0..W).map(|_| 3.0 * normal(&mut rng)).collect();
        let r = compare(&span(&quiet, &loud)).unwrap();
        assert!(r.variance_ratio() > VARIANCE_RATIO, "{r:?}");
        assert!(r.autocorrelation_rise() <= AUTOCORRELATION_RISE, "{r:?}");
        assert!(!r.warns(), "{r:?}");
        // Memory up, variance the same: an alternating start, a smooth
        // wave at the end scaled to the same variance.
        let first: Vec<f64> = (0..W)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let second: Vec<f64> = (0..W)
            .map(|i| (i as f64 * 0.3).sin() * std::f64::consts::SQRT_2)
            .collect();
        let r = compare(&span(&first, &second)).unwrap();
        assert!(r.autocorrelation_rise() > AUTOCORRELATION_RISE, "{r:?}");
        assert!(!r.warns(), "{r:?}");
    }
}

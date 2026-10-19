//! Critical slowing down: a system nearing a tipping point recovers more
//! slowly from small shocks, so its readings wander further and remember
//! more of the step before. Over the last `2W` plausible reports, compare
//! the variance and the lag-1 autocorrelation of the first half with the
//! second. Both rising past their margins is a warning; the first report
//! of such an episode asks. Pure: numbers in, a verdict out.

/// The half window, in reports.
pub const W: usize = 20;
/// The second half's variance must exceed the first's by this factor.
pub const VARIANCE_RATIO: f64 = 1.5;
/// The second half's lag-1 autocorrelation must exceed the first's by this.
pub const AUTOCORRELATION_RISE: f64 = 0.2;

/// The variance of a sample about its mean, and its lag-1 autocorrelation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    pub variance: f64,
    pub autocorrelation: f64,
}

/// The stats of one half. None for fewer than three values.
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

/// What a warning says: how the halves compare.
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

/// The halves of the last `2W` values, oldest first. None with fewer.
pub fn compare(series: &[f64]) -> Option<Rise> {
    if series.len() < 2 * W {
        return None;
    }
    let last = &series[series.len() - 2 * W..];
    Some(Rise {
        first: stats(&last[..W])?,
        second: stats(&last[W..])?,
    })
}

/// Does the latest value start an episode: a warning now, and none at the
/// value before? Returns what warns.
pub fn starts(series: &[f64]) -> Option<Rise> {
    let now = compare(series).filter(Rise::warns)?;
    let before = series
        .len()
        .checked_sub(1)
        .and_then(|n| compare(&series[..n]))
        .is_some_and(|r| r.warns());
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

    #[test]
    fn stats_of_known_values() {
        let s = stats(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert!((s.variance - 1.25).abs() < 1e-12);
        // Deviations -1.5 -.5 .5 1.5: (.75 - .25 + .75) / 5 = .25.
        assert!((s.autocorrelation - 0.25).abs() < 1e-12, "{s:?}");
        assert_eq!(stats(&[5.0; 4]).unwrap().autocorrelation, 0.0);
        assert!(stats(&[1.0, 2.0]).is_none());
        assert!(compare(&[1.0; 2 * W - 1]).is_none());
    }

    #[test]
    fn a_ramping_ar1_warns_once_and_white_noise_does_not() {
        let series = ramp(2 * W + 20, 7);
        let n = episodes(&series);
        let last = compare(&series).unwrap();
        assert_eq!(
            n,
            1,
            "variance x{:.2}, autocorrelation {:+.2}",
            last.variance_ratio(),
            last.autocorrelation_rise()
        );
        let noise = white(2 * W + 20, 7);
        assert_eq!(episodes(&noise), 0);
    }

    /// Over 200 seeds of 60 reports: the ramp warns in nearly every one;
    /// white noise sometimes does, 2W being a short window.
    #[test]
    fn over_many_seeds_the_ramp_warns_and_noise_mostly_does_not() {
        let warned = |f: fn(usize, u64) -> Vec<f64>| {
            (0..200u64)
                .filter(|s| episodes(&f(2 * W + 20, *s)) > 0)
                .count()
        };
        let (r, w) = (warned(ramp), warned(white));
        assert!(r >= 180, "ramp warned in {r} of 200");
        assert!(w <= 60, "white noise warned in {w} of 200");
    }

    /// An episode is a run of reports that warn; only its first starts it.
    #[test]
    fn an_episode_starts_on_its_first_warning_report_only() {
        let long = (0..50u64)
            .map(|seed| ramp(2 * W + 40, seed))
            .find(|s| {
                let warning = (1..=s.len())
                    .filter(|&n| compare(&s[..n]).is_some_and(|r| r.warns()))
                    .count();
                warning > episodes(s)
            })
            .expect("a ramp whose warning lasts more than one report");
        let warns = |n: usize| compare(&long[..n]).is_some_and(|r| r.warns());
        for n in 1..=long.len() {
            assert_eq!(
                starts(&long[..n]).is_some(),
                warns(n) && !warns(n - 1),
                "report {n}"
            );
        }
    }

    #[test]
    fn rising_variance_alone_or_rising_autocorrelation_alone_does_not_warn() {
        // Variance up fourfold, both halves white: no memory gained.
        let mut rng = 3;
        let mut louder: Vec<f64> = (0..W).map(|_| normal(&mut rng)).collect();
        louder.extend((0..W).map(|_| 2.0 * normal(&mut rng)));
        let r = compare(&louder).unwrap();
        assert!(r.variance_ratio() > VARIANCE_RATIO, "{r:?}");
        assert!(!r.warns(), "{r:?}");
        // Memory up, variance the same: a smooth wave in the second half
        // scaled to the first half's variance.
        let first: Vec<f64> = (0..W)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let second: Vec<f64> = (0..W)
            .map(|i| (i as f64 * 0.3).sin() * std::f64::consts::SQRT_2)
            .collect();
        let both: Vec<f64> = first.into_iter().chain(second).collect();
        let r = compare(&both).unwrap();
        assert!(r.autocorrelation_rise() > AUTOCORRELATION_RISE, "{r:?}");
        assert!(!r.warns(), "{r:?}");
    }
}

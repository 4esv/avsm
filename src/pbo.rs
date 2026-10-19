//! The probability of backtest overfitting, by combinatorially symmetric
//! cross-validation (Bailey, Borwein, López de Prado & Zhu, 2014).
//!
//! The matrix holds one row per scenario (a seed) and one column per
//! variant (a value tried), higher being better. The rows are cut into S
//! groups in order; every way of taking half of them as in-sample, the
//! rest out-of-sample, picks the variant best in-sample and asks where it
//! ranks out-of-sample. λ is the logit of that relative rank, and PBO the
//! share of splits with λ ≤ 0: how often the in-sample winner is no better
//! than the median out of it. Pure code, no model.

/// Row groups at most; fewer when there are fewer rows.
pub const GROUPS: usize = 8;

/// What CSCV found.
#[derive(Clone, Debug, PartialEq)]
pub struct Pbo {
    /// The share of splits whose in-sample best ranks at or below the
    /// median out of sample.
    pub pbo: f64,
    /// Row groups used: even, at most `GROUPS`.
    pub groups: usize,
    /// C(groups, groups / 2).
    pub splits: usize,
    /// λ of each split, in the order the splits were taken.
    pub logits: Vec<f64>,
}

/// CSCV over `matrix`, rows by columns. None with fewer than two rows or
/// two columns, a ragged matrix, or a value that is not finite.
pub fn cscv(matrix: &[Vec<f64>]) -> Option<Pbo> {
    let rows = matrix.len();
    let cols = matrix.first()?.len();
    if rows < 2 || cols < 2 {
        return None;
    }
    if matrix
        .iter()
        .any(|r| r.len() != cols || r.iter().any(|x| !x.is_finite()))
    {
        return None;
    }
    let mut groups = GROUPS.min(rows);
    groups -= groups % 2;
    // Contiguous groups whose sizes differ by at most one.
    let bounds: Vec<(usize, usize)> = (0..groups)
        .map(|g| (g * rows / groups, (g + 1) * rows / groups))
        .collect();
    let mean = |mask: u32, inside: bool, col: usize| -> f64 {
        let mut sum = 0.0;
        let mut n = 0usize;
        for (g, (lo, hi)) in bounds.iter().enumerate() {
            if (mask >> g & 1 == 1) == inside {
                for row in &matrix[*lo..*hi] {
                    sum += row[col];
                    n += 1;
                }
            }
        }
        sum / n as f64
    };
    let mut logits = Vec::new();
    for mask in 0u32..(1 << groups) {
        if mask.count_ones() as usize != groups / 2 {
            continue;
        }
        let is: Vec<f64> = (0..cols).map(|c| mean(mask, true, c)).collect();
        let oos: Vec<f64> = (0..cols).map(|c| mean(mask, false, c)).collect();
        let best = (0..cols).fold(0, |b, c| if is[c] > is[b] { c } else { b });
        let below = oos.iter().filter(|x| **x < oos[best]).count() as f64;
        let ties = oos.iter().filter(|x| **x == oos[best]).count() as f64 - 1.0;
        let rank = 1.0 + below + ties / 2.0;
        let omega = rank / (cols as f64 + 1.0);
        logits.push((omega / (1.0 - omega)).ln());
    }
    let splits = logits.len();
    let overfit = logits.iter().filter(|l| **l <= 0.0).count();
    Some(Pbo {
        pbo: overfit as f64 / splits as f64,
        groups,
        splits,
        logits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::splitmix64;

    fn noise(rows: usize, cols: usize, seed: u64) -> Vec<Vec<f64>> {
        let mut s = seed;
        (0..rows)
            .map(|_| {
                (0..cols)
                    .map(|_| (splitmix64(&mut s) >> 11) as f64 / (1u64 << 53) as f64)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn eight_groups_give_seventy_splits() {
        let p = cscv(&noise(16, 3, 1)).unwrap();
        assert_eq!((p.groups, p.splits, p.logits.len()), (8, 70, 70));
        // Fewer rows, fewer groups, and always an even number.
        assert_eq!(cscv(&noise(5, 3, 1)).unwrap().groups, 4);
        assert_eq!(cscv(&noise(5, 3, 1)).unwrap().splits, 6);
        assert_eq!(cscv(&noise(3, 3, 1)).unwrap().splits, 2);
        assert!(cscv(&noise(1, 3, 1)).is_none());
        assert!(cscv(&noise(16, 1, 1)).is_none());
        assert!(cscv(&[vec![1.0, 2.0], vec![1.0]]).is_none());
        assert!(cscv(&[vec![1.0, f64::NAN], vec![1.0, 2.0]]).is_none());
    }

    #[test]
    fn when_the_in_sample_best_is_pure_noise_pbo_is_near_a_half_or_more() {
        // Every variant is the same noise: being best in sample says
        // nothing out of it. Averaged over several draws so one lucky
        // matrix cannot carry it.
        let draws: Vec<f64> = (0..20)
            .map(|seed| cscv(&noise(16, 10, seed)).unwrap().pbo)
            .collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!(mean >= 0.4, "mean PBO {mean} over {draws:?}");
    }

    #[test]
    fn a_truly_dominant_variant_has_pbo_near_zero() {
        let mut m = noise(16, 10, 7);
        for row in &mut m {
            row[3] += 5.0;
        }
        let p = cscv(&m).unwrap();
        assert_eq!(p.pbo, 0.0, "{p:?}");
        assert!(p.logits.iter().all(|l| *l > 0.0));
    }

    #[test]
    fn the_variant_is_picked_in_sample_and_ranked_out_of_it() {
        // Four groups of one row. The first split holds rows 0 and 1 in
        // sample: variant 0 is best there and worst in rows 2 and 3, so
        // its lambda is below 0. Picked in rows 2 and 3 instead, variant 1
        // would rank third of four in rows 0 and 1, above 0: PBO summed
        // over every split cannot tell the two apart, this split can.
        let m = vec![
            vec![4.0, 3.0, 0.0, 0.0],
            vec![4.0, 3.0, 0.0, 0.0],
            vec![0.0, 3.0, 2.0, 1.0],
            vec![0.0, 3.0, 2.0, 1.0],
        ];
        let p = cscv(&m).unwrap();
        assert_eq!(p.splits, 6);
        assert!((p.logits[0] - (1.0f64 / 4.0).ln()).abs() < 1e-12, "{p:?}");
    }

    #[test]
    fn the_logit_is_of_the_relative_rank_out_of_sample() {
        // Two groups of one row each. In the split where row 0 is in
        // sample, variant 0 is best there and worst in row 1: rank 1 of 2,
        // omega 1/3, lambda ln(1/2). The other split mirrors it.
        let p = cscv(&[vec![2.0, 1.0], vec![1.0, 2.0]]).unwrap();
        assert_eq!(p.splits, 2);
        for l in &p.logits {
            assert!((l - 0.5f64.ln()).abs() < 1e-12, "{l}");
        }
        assert_eq!(p.pbo, 1.0);
    }
}

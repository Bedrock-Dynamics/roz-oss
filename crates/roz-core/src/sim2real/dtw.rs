use serde::{Deserialize, Serialize};

/// Result of dynamic time warping alignment between two time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DtwResult {
    /// Total warping distance normalized by path length.
    pub distance: f64,
    /// The warping path as pairs of `(sim_index, real_index)`.
    pub warping_path: Vec<(usize, usize)>,
}

/// Maximum number of cells in the DTW cost matrix (~80 MB for f64).
const MAX_DTW_CELLS: usize = 10_000_000;

/// Compute Dynamic Time Warping between two 1-D time series.
///
/// `band_width` applies a Sakoe-Chiba band constraint limiting how far
/// the warping path can deviate from the diagonal.  Pass `None` for
/// unconstrained DTW.
///
/// Returns a `DtwResult` with the normalized distance and the optimal
/// warping path.  If either input is empty the distance is 0 and the
/// path is empty.
pub fn dtw_align(sim_series: &[f64], real_series: &[f64], band_width: Option<usize>) -> DtwResult {
    let n = sim_series.len();
    let m = real_series.len();

    if n == 0 || m == 0 {
        return DtwResult {
            distance: 0.0,
            warping_path: Vec::new(),
        };
    }

    // Guard against excessive memory allocation
    if n.saturating_mul(m) > MAX_DTW_CELLS {
        return DtwResult {
            distance: f64::INFINITY,
            warping_path: Vec::new(),
        };
    }

    // Build cost matrix
    let mut cost = vec![vec![f64::INFINITY; m]; n];

    let in_band = |i: usize, j: usize| -> bool { band_width.is_none_or(|w| i.abs_diff(j) <= w) };

    // Fill the cost matrix via dynamic programming
    for i in 0..n {
        for j in 0..m {
            if !in_band(i, j) {
                continue;
            }
            let d = (sim_series[i] - real_series[j]).abs();
            let prev = match (i, j) {
                (0, 0) => 0.0,
                (0, _) => cost[0][j - 1],
                (_, 0) => cost[i - 1][0],
                _ => cost[i - 1][j].min(cost[i][j - 1]).min(cost[i - 1][j - 1]),
            };
            cost[i][j] = d + prev;
        }
    }

    // Backtrack to find the warping path
    let mut path = Vec::new();
    let mut i = n - 1;
    let mut j = m - 1;
    path.push((i, j));

    while i > 0 || j > 0 {
        if i == 0 {
            j -= 1;
        } else if j == 0 {
            i -= 1;
        } else {
            let diag = cost[i - 1][j - 1];
            let left = cost[i][j - 1];
            let up = cost[i - 1][j];
            if diag <= left && diag <= up {
                i -= 1;
                j -= 1;
            } else if up <= left {
                i -= 1;
            } else {
                j -= 1;
            }
        }
        path.push((i, j));
    }

    path.reverse();

    let path_len = path.len();
    #[expect(
        clippy::cast_precision_loss,
        reason = "path length will never exceed f64 mantissa range"
    )]
    let normalized = if path_len > 0 {
        cost[n - 1][m - 1] / path_len as f64
    } else {
        0.0
    };

    DtwResult {
        distance: normalized,
        warping_path: path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_series_zero_distance() {
        let s = vec![1.0, 2.0, 3.0, 4.0];
        let result = dtw_align(&s, &s, None);
        assert!((result.distance).abs() < f64::EPSILON);
    }

    #[test]
    fn shifted_series() {
        let sim = vec![0.0, 1.0, 2.0, 3.0];
        let real = vec![1.0, 2.0, 3.0, 4.0];
        let result = dtw_align(&sim, &real, None);
        assert!(result.distance > 0.0);
        assert!(!result.warping_path.is_empty());
    }

    #[test]
    fn different_lengths() {
        let sim = vec![1.0, 2.0, 3.0];
        let real = vec![1.0, 1.5, 2.0, 2.5, 3.0];
        let result = dtw_align(&sim, &real, None);
        assert!(result.distance >= 0.0);
        // Path must start at (0,0) and end at (2,4)
        assert_eq!(result.warping_path[0], (0, 0));
        assert_eq!(*result.warping_path.last().unwrap(), (2, 4));
    }

    #[test]
    fn band_width_constraint() {
        let sim = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let real = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let result = dtw_align(&sim, &real, Some(1));
        assert!((result.distance).abs() < f64::EPSILON);
        // All path elements should be within band
        for &(i, j) in &result.warping_path {
            let diff = if i > j { i - j } else { j - i };
            assert!(diff <= 1);
        }
    }

    #[test]
    fn single_element_series() {
        let result = dtw_align(&[5.0], &[3.0], None);
        assert!((result.distance - 2.0).abs() < f64::EPSILON);
        assert_eq!(result.warping_path, vec![(0, 0)]);
    }

    #[test]
    fn empty_series_handling() {
        let result = dtw_align(&[], &[1.0, 2.0], None);
        assert!((result.distance).abs() < f64::EPSILON);
        assert!(result.warping_path.is_empty());
    }

    #[test]
    fn empty_both_series() {
        let result = dtw_align(&[], &[], None);
        assert!((result.distance).abs() < f64::EPSILON);
        assert!(result.warping_path.is_empty());
    }

    #[test]
    fn oversized_inputs_return_infinity() {
        // 10001 x 1001 = 10_011_001 > 10_000_000
        let large_a: Vec<f64> = (0..10_001).map(|i| i as f64).collect();
        let large_b: Vec<f64> = (0..1_001).map(|i| i as f64).collect();
        let result = dtw_align(&large_a, &large_b, None);
        assert!(result.distance.is_infinite());
        assert!(result.warping_path.is_empty());
    }

    #[test]
    fn path_is_monotonically_increasing() {
        let sim = vec![1.0, 3.0, 5.0, 7.0];
        let real = vec![2.0, 4.0, 6.0];
        let result = dtw_align(&sim, &real, None);
        for window in result.warping_path.windows(2) {
            assert!(window[1].0 >= window[0].0);
            assert!(window[1].1 >= window[0].1);
        }
    }
}

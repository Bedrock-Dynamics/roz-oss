/// Compute the root mean squared error between two equal-length series.
///
/// Returns `None` if the inputs are empty or have different lengths.
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts will never exceed f64 mantissa range"
)]
pub fn rmse(sim: &[f64], real: &[f64]) -> Option<f64> {
    if sim.is_empty() || sim.len() != real.len() {
        return None;
    }
    let sum_sq: f64 = sim.iter().zip(real).map(|(s, r)| (s - r).powi(2)).sum();
    Some((sum_sq / sim.len() as f64).sqrt())
}

/// Compute the mean absolute error between two equal-length series.
///
/// Returns `None` if the inputs are empty or have different lengths.
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts will never exceed f64 mantissa range"
)]
pub fn mae(sim: &[f64], real: &[f64]) -> Option<f64> {
    if sim.is_empty() || sim.len() != real.len() {
        return None;
    }
    let sum_abs: f64 = sim.iter().zip(real).map(|(s, r)| (s - r).abs()).sum();
    Some(sum_abs / sim.len() as f64)
}

/// Return the maximum absolute difference between two equal-length series.
///
/// Returns `None` if the inputs are empty or have different lengths.
pub fn max_deviation(sim: &[f64], real: &[f64]) -> Option<f64> {
    if sim.is_empty() || sim.len() != real.len() {
        return None;
    }
    sim.iter().zip(real).map(|(s, r)| (s - r).abs()).reduce(f64::max)
}

/// RMSE normalized by the range of the real data.
///
/// Returns `None` if the inputs are empty, have different lengths,
/// or the real data has zero range.
pub fn normalized_rmse(sim: &[f64], real: &[f64]) -> Option<f64> {
    let r = rmse(sim, real)?;

    let min = real.iter().copied().reduce(f64::min)?;
    let max = real.iter().copied().reduce(f64::max)?;
    let range = max - min;

    if range == 0.0 {
        return None;
    }

    Some(r / range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_error_for_identical_data() {
        let data = vec![1.0, 2.0, 3.0, 4.0];
        assert!((rmse(&data, &data).unwrap()).abs() < f64::EPSILON);
        assert!((mae(&data, &data).unwrap()).abs() < f64::EPSILON);
        assert!((max_deviation(&data, &data).unwrap()).abs() < f64::EPSILON);
    }

    #[test]
    fn known_rmse_value() {
        let sim = vec![1.0, 2.0, 3.0];
        let real = vec![2.0, 3.0, 4.0];
        // Each diff = 1, squared = 1, mean = 1, sqrt = 1
        assert!((rmse(&sim, &real).unwrap() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn known_mae_value() {
        let sim = vec![1.0, 2.0, 3.0];
        let real = vec![2.0, 4.0, 6.0];
        // Diffs: 1, 2, 3 -> mean = 2
        assert!((mae(&sim, &real).unwrap() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn known_max_deviation() {
        let sim = vec![1.0, 2.0, 3.0];
        let real = vec![1.0, 2.0, 10.0];
        assert!((max_deviation(&sim, &real).unwrap() - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_inputs_return_none() {
        assert!(rmse(&[], &[]).is_none());
        assert!(mae(&[], &[]).is_none());
        assert!(max_deviation(&[], &[]).is_none());
        assert!(normalized_rmse(&[], &[]).is_none());
    }

    #[test]
    fn mismatched_lengths_return_none() {
        assert!(rmse(&[1.0], &[1.0, 2.0]).is_none());
        assert!(mae(&[1.0, 2.0], &[1.0]).is_none());
        assert!(max_deviation(&[1.0], &[1.0, 2.0]).is_none());
    }

    #[test]
    fn normalized_rmse_calculation() {
        let sim = vec![1.0, 2.0, 3.0];
        let real = vec![0.0, 5.0, 10.0];
        let nrmse = normalized_rmse(&sim, &real).unwrap();
        let expected_rmse = rmse(&sim, &real).unwrap();
        let range = 10.0; // max(real) - min(real) = 10 - 0 = 10
        assert!((nrmse - expected_rmse / range).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_rmse_zero_range_returns_none() {
        let sim = vec![1.0, 2.0];
        let real = vec![5.0, 5.0]; // zero range
        assert!(normalized_rmse(&sim, &real).is_none());
    }

    #[test]
    fn single_element() {
        assert!((rmse(&[3.0], &[5.0]).unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((mae(&[3.0], &[5.0]).unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((max_deviation(&[3.0], &[5.0]).unwrap() - 2.0).abs() < f64::EPSILON);
    }
}

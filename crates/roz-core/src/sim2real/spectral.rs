/// Compute a spectral-coherence proxy between two equal-length signals.
///
/// Uses the absolute value of the Pearson correlation coefficient as
/// an FFT-free coherence approximation.  The result is clamped to
/// \[0.0, 1.0\].
///
/// Returns `None` if inputs are empty, have different lengths, or
/// either signal has zero variance (constant).
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts will never exceed f64 mantissa range"
)]
pub fn spectral_coherence(sim: &[f64], real: &[f64]) -> Option<f64> {
    let n = sim.len();
    if n == 0 || n != real.len() {
        return None;
    }

    let mean_s: f64 = sim.iter().sum::<f64>() / n as f64;
    let mean_r: f64 = real.iter().sum::<f64>() / n as f64;

    let mut cov = 0.0_f64;
    let mut var_s = 0.0_f64;
    let mut var_r = 0.0_f64;

    for i in 0..n {
        let ds = sim[i] - mean_s;
        let dr = real[i] - mean_r;
        cov += ds * dr;
        var_s += ds * ds;
        var_r += dr * dr;
    }

    // Zero variance in either signal means correlation is undefined
    if var_s == 0.0 || var_r == 0.0 {
        return None;
    }

    let r = cov / (var_s.sqrt() * var_r.sqrt());
    Some(r.abs().clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_signals_return_one() {
        let s = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let c = spectral_coherence(&s, &s).unwrap();
        assert!((c - 1.0).abs() < 1e-10);
    }

    #[test]
    fn opposite_signals_return_one() {
        let sim = vec![1.0, 2.0, 3.0, 4.0];
        let real = vec![-1.0, -2.0, -3.0, -4.0];
        let c = spectral_coherence(&sim, &real).unwrap();
        assert!((c - 1.0).abs() < 1e-10);
    }

    #[test]
    fn linearly_related_signals() {
        let sim = vec![0.0, 1.0, 2.0, 3.0];
        let real: Vec<f64> = sim.iter().map(|x| 2.0 * x + 5.0).collect();
        let c = spectral_coherence(&sim, &real).unwrap();
        assert!((c - 1.0).abs() < 1e-10);
    }

    #[test]
    fn constant_signals_return_none() {
        let s = vec![3.0, 3.0, 3.0, 3.0];
        assert!(spectral_coherence(&s, &s).is_none());
    }

    #[test]
    fn different_lengths_return_none() {
        assert!(spectral_coherence(&[1.0, 2.0], &[1.0]).is_none());
    }

    #[test]
    fn empty_inputs_return_none() {
        assert!(spectral_coherence(&[], &[]).is_none());
    }
}

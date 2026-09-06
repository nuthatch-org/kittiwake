//! Latency measurement that does not flatter itself.
//!
//! Two rules, both learned the hard way. A soak driven through the service exercises the service's
//! own cache after the first pass rather than the nest, so cold and warm are measured separately
//! and reported separately. And a mean hides the case that matters: a p50 that improves while p99
//! degrades is a regression for whoever hits the p99.

/// Latencies in milliseconds, sorted on construction.
pub struct Sample(Vec<f64>);

impl Sample {
    pub fn new(mut values: Vec<f64>) -> Self {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Self(values)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Nearest-rank percentile. No interpolation: with a few hundred samples, inventing a value
    /// between two measurements is a worse answer than picking the measurement.
    pub fn p(&self, q: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let rank = ((q / 100.0) * self.0.len() as f64).ceil().max(1.0) as usize;
        self.0[rank.min(self.0.len()) - 1]
    }

    pub fn max(&self) -> f64 {
        self.0.last().copied().unwrap_or(f64::NAN)
    }

    pub fn mean(&self) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        self.0.iter().sum::<f64>() / self.0.len() as f64
    }
}

/// A millisecond figure at the precision it was actually measured to.
pub fn ms(v: f64) -> String {
    if v.is_nan() {
        "-".into()
    } else if v < 10.0 {
        format!("{v:.2} ms")
    } else {
        format!("{v:.0} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_pick_a_real_measurement() {
        let s = Sample::new(vec![5.0, 1.0, 4.0, 2.0, 3.0]);
        assert_eq!(s.p(50.0), 3.0);
        assert_eq!(s.p(99.0), 5.0);
        assert_eq!(s.max(), 5.0);
        assert_eq!(s.p(100.0), 5.0);
    }

    #[test]
    fn one_slow_call_does_not_hide_behind_the_mean() {
        let s = Sample::new(vec![1.0; 99].into_iter().chain([1000.0]).collect());
        assert!(s.mean() < 12.0, "the mean says everything is fine");
        assert_eq!(s.p(99.0), 1.0);
        assert_eq!(s.max(), 1000.0, "the max is where the truth is");
    }

    #[test]
    fn an_empty_sample_reports_nothing_rather_than_zero() {
        let s = Sample::new(vec![]);
        assert!(s.p(50.0).is_nan());
        assert_eq!(ms(s.p(50.0)), "-");
    }
}

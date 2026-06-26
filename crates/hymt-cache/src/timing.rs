use crate::history::format_duration;

const MIN_MEANINGFUL_ESTIMATE_SECONDS: f64 = 0.1;

/// Data captured at translation completion for timing divergence analysis.
#[derive(Debug, Clone)]
pub struct TimingIssueData {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub segments: i64,
    pub actual_seconds: f64,
    pub estimated_seconds: f64,
    pub config_version: i64,
    pub target_lang: String,
    pub template_type: String,
    pub concurrency: i64,
    pub model: Option<String>,
}

impl TimingIssueData {
    /// Ratio of actual to estimated duration.
    ///
    /// Returns `0.0` when `estimated_seconds` is too small to be meaningful.
    pub fn ratio(&self) -> f64 {
        if self.estimated_seconds < MIN_MEANINGFUL_ESTIMATE_SECONDS {
            return 0.0;
        }
        self.actual_seconds / self.estimated_seconds
    }
}

/// Return `true` when the actual/estimated ratio exceeds the threshold in either direction.
///
/// `threshold` is clamped to a minimum of `2.0` — values ≤ 1.0 are nonsensical for
/// divergence detection and default to 2.0 (i.e. 2× in either direction).
///
/// A ratio of `0.0` (no meaningful estimate) is never considered divergent.
pub fn is_divergent(data: &TimingIssueData, threshold: f64) -> bool {
    let effective = if threshold > 1.0 { threshold } else { 2.0 };
    let ratio = data.ratio();
    if ratio == 0.0 {
        // No meaningful estimate — cannot determine divergence
        return false;
    }
    ratio > effective || ratio < 1.0 / effective
}

/// Format a brief human-readable timing report.
pub fn format_timing_report(data: &TimingIssueData) -> String {
    format!(
        "Actual: {} vs Estimated: {} (ratio: {:.2}x)",
        format_duration(data.actual_seconds),
        format_duration(data.estimated_seconds),
        data.ratio()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(actual: f64, estimated: f64) -> TimingIssueData {
        TimingIssueData {
            input_tokens: 100,
            output_tokens: 200,
            segments: 4,
            actual_seconds: actual,
            estimated_seconds: estimated,
            config_version: 1,
            target_lang: "en".to_owned(),
            template_type: "default".to_owned(),
            concurrency: 2,
            model: None,
        }
    }

    #[test]
    fn test_not_divergent_within_threshold() {
        let data = make_data(10.0, 12.0); // ratio ≈ 0.83, within 2×
        assert!(!is_divergent(&data, 2.0));
    }

    #[test]
    fn test_divergent_actual_too_high() {
        let data = make_data(30.0, 10.0); // ratio = 3.0 > 2.0
        assert!(is_divergent(&data, 2.0));
    }

    #[test]
    fn test_divergent_actual_too_low() {
        let data = make_data(3.0, 10.0); // ratio = 0.3 < 0.5
        assert!(is_divergent(&data, 2.0));
    }

    #[test]
    fn test_zero_estimated_not_divergent() {
        // Issue #53 context: when estimated = 0 (no history), ratio = 0 → not divergent
        let data = make_data(20.0, 0.0);
        assert!(!is_divergent(&data, 2.0));
    }

    #[test]
    fn test_sub_display_estimate_not_divergent() {
        let data = make_data(6.4, 0.04);
        assert_eq!(data.ratio(), 0.0);
        assert!(!is_divergent(&data, 2.0));
    }

    #[test]
    fn test_minimum_threshold_is_2() {
        // threshold ≤ 1.0 → treated as 2.0
        let data = make_data(30.0, 10.0); // ratio = 3.0
                                          // With real threshold 0.5 (invalid), effective = 2.0 → divergent
        assert!(is_divergent(&data, 0.5));
        // ratio = 3.0 with effective=2.0: divergent
        assert!(is_divergent(&data, 1.0));
    }

    #[test]
    fn test_threshold_1_5_not_divergent() {
        let data = make_data(14.0, 10.0); // ratio = 1.4 < 1.5
        assert!(!is_divergent(&data, 1.5));
    }

    #[test]
    fn test_threshold_1_5_divergent() {
        let data = make_data(16.0, 10.0); // ratio = 1.6 > 1.5
        assert!(is_divergent(&data, 1.5));
    }

    #[test]
    fn test_ratio_zero_when_no_estimate() {
        let data = make_data(5.0, 0.0);
        assert_eq!(data.ratio(), 0.0);
    }

    #[test]
    fn test_ratio_computed_correctly() {
        let data = make_data(15.0, 10.0);
        assert!((data.ratio() - 1.5).abs() < 1e-10);
    }

    #[test]
    fn test_format_timing_report() {
        let data = make_data(20.0, 10.0);
        let report = format_timing_report(&data);
        assert!(report.contains("20s"), "report was: {report}");
        assert!(report.contains("10s"), "report was: {report}");
        assert!(report.contains("2.00x"), "report was: {report}");
    }

    #[test]
    fn test_format_timing_report_no_estimate() {
        let data = make_data(30.0, 0.0);
        let report = format_timing_report(&data);
        // estimated = 0 → "0s", ratio = 0.00x
        assert!(report.contains("0.00x"), "report was: {report}");
    }
}

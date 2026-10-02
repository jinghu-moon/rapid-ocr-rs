//! 共享的耗时采样统计。
//!
//! benchmark 与评测工具都要报告“不只看平均值”的延迟分布，因此把统计口径放在
//! 共享层，避免每个 bin 各写一套 percentile。

use serde::Serialize;

/// 一个指标的采样统计（毫秒）。
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub samples: usize,
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub stddev_ms: f64,
}

impl Stats {
    pub fn from_samples(mut values: Vec<f64>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_by(f64::total_cmp);
        let samples = values.len();
        let mean = values.iter().sum::<f64>() / samples as f64;
        let variance = if samples > 1 {
            values
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / (samples - 1) as f64
        } else {
            0.0
        };
        Self {
            samples,
            min_ms: values[0],
            max_ms: values[samples - 1],
            mean_ms: mean,
            p50_ms: percentile(&values, 0.50),
            p95_ms: percentile(&values, 0.95),
            stddev_ms: variance.sqrt(),
        }
    }
}

/// 线性插值百分位，与 numpy `percentile` 默认插值一致。
pub fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let position = fraction * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

#[cfg(test)]
mod tests {
    use super::{Stats, percentile};

    #[test]
    fn percentile_matches_linear_interpolation() {
        let values = vec![1.0, 2.0, 3.0, 4.0];
        assert!((percentile(&values, 0.0) - 1.0).abs() < 1e-12);
        assert!((percentile(&values, 1.0) - 4.0).abs() < 1e-12);
        assert!((percentile(&values, 0.5) - 2.5).abs() < 1e-12);
        assert!((percentile(&values, 0.95) - 3.85).abs() < 1e-12);
    }

    #[test]
    fn stats_reports_min_max_mean_and_stddev() {
        let stats = Stats::from_samples(vec![4.0, 1.0, 3.0, 2.0]);
        assert_eq!(stats.samples, 4);
        assert!((stats.min_ms - 1.0).abs() < 1e-12);
        assert!((stats.max_ms - 4.0).abs() < 1e-12);
        assert!((stats.mean_ms - 2.5).abs() < 1e-12);
        assert!((stats.p50_ms - 2.5).abs() < 1e-12);
        assert!(stats.stddev_ms > 0.0);
    }

    #[test]
    fn empty_stats_are_zeroed() {
        let stats = Stats::from_samples(Vec::new());
        assert_eq!(stats.samples, 0);
        assert_eq!(stats.mean_ms, 0.0);
        assert_eq!(stats.p95_ms, 0.0);
    }
}

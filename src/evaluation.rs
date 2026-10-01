use serde::{Deserialize, Serialize};

use crate::OcrJsonItem;

#[derive(Debug, Clone, Deserialize)]
pub struct EvaluationCase {
    pub image: String,
    pub text: String,
    #[serde(default)]
    pub boxes: Vec<[[f32; 2]; 4]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluationMetrics {
    pub cer: f32,
    pub detection_precision: Option<f32>,
    pub detection_recall: Option<f32>,
    pub mean_polygon_iou: Option<f32>,
    pub exact_text: bool,
}

/// Evaluation result associated with its source image.
///
/// Keeping the image name beside the metrics is important for regression
/// reports: an array of anonymous metric objects cannot be traced back to a
/// failing fixture when a manifest contains more than one image.
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationReport {
    pub image: String,
    #[serde(flatten)]
    pub metrics: EvaluationMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluationSummary {
    pub cases: Vec<EvaluationReport>,
    pub mean_cer: f32,
    pub exact_match_rate: f32,
    pub mean_detection_precision: Option<f32>,
    pub mean_detection_recall: Option<f32>,
    pub mean_polygon_iou: Option<f32>,
}

impl EvaluationSummary {
    pub fn from_cases(cases: Vec<EvaluationReport>) -> Self {
        let count = cases.len() as f32;
        let mean = |values: Vec<f32>| {
            (!values.is_empty()).then(|| values.iter().sum::<f32>() / values.len() as f32)
        };
        Self {
            mean_cer: if count == 0.0 {
                0.0
            } else {
                cases.iter().map(|case| case.metrics.cer).sum::<f32>() / count
            },
            exact_match_rate: if count == 0.0 {
                0.0
            } else {
                cases.iter().filter(|case| case.metrics.exact_text).count() as f32 / count
            },
            mean_detection_precision: mean(
                cases
                    .iter()
                    .filter_map(|case| case.metrics.detection_precision)
                    .collect(),
            ),
            mean_detection_recall: mean(
                cases
                    .iter()
                    .filter_map(|case| case.metrics.detection_recall)
                    .collect(),
            ),
            mean_polygon_iou: mean(
                cases
                    .iter()
                    .filter_map(|case| case.metrics.mean_polygon_iou)
                    .collect(),
            ),
            cases,
        }
    }
}

pub fn character_error_rate(reference: &str, hypothesis: &str) -> f32 {
    let reference: Vec<char> = normalize_text(reference).chars().collect();
    let hypothesis: Vec<char> = normalize_text(hypothesis).chars().collect();
    if reference.is_empty() {
        return if hypothesis.is_empty() { 0.0 } else { 1.0 };
    }
    let mut previous: Vec<usize> = (0..=hypothesis.len()).collect();
    for (row, expected) in reference.iter().enumerate() {
        let mut current = vec![row + 1; hypothesis.len() + 1];
        for (column, actual) in hypothesis.iter().enumerate() {
            current[column + 1] = if expected == actual {
                previous[column]
            } else {
                1 + previous[column]
                    .min(previous[column + 1])
                    .min(current[column])
            };
        }
        previous = current;
    }
    previous[hypothesis.len()] as f32 / reference.len() as f32
}

pub fn evaluate_case(
    reference_text: &str,
    expected_boxes: &[[[f32; 2]; 4]],
    predicted: &[OcrJsonItem],
    iou_threshold: f32,
) -> EvaluationMetrics {
    let predicted_text = predicted
        .iter()
        .map(|item| item.txt.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let matches = match_boxes(expected_boxes, predicted, iou_threshold);
    let detection_recall =
        (!expected_boxes.is_empty()).then(|| matches.len() as f32 / expected_boxes.len() as f32);
    let detection_precision = (!expected_boxes.is_empty() && !predicted.is_empty())
        .then(|| matches.len() as f32 / predicted.len() as f32);
    let mean_polygon_iou = (!matches.is_empty())
        .then(|| matches.iter().map(|(_, _, iou)| *iou).sum::<f32>() / matches.len() as f32);
    EvaluationMetrics {
        cer: character_error_rate(reference_text, &predicted_text),
        detection_precision,
        detection_recall,
        mean_polygon_iou,
        exact_text: normalize_text(reference_text) == normalize_text(&predicted_text),
    }
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn match_boxes(
    expected: &[[[f32; 2]; 4]],
    predicted: &[OcrJsonItem],
    threshold: f32,
) -> Vec<(usize, usize, f32)> {
    let mut candidates = Vec::new();
    for (expected_index, expected_box) in expected.iter().enumerate() {
        for (predicted_index, predicted_item) in predicted.iter().enumerate() {
            if let Some(predicted_box) = predicted_item.box_ {
                let iou = polygon_iou(
                    *expected_box,
                    predicted_box.map(|point| [point[0] as f32, point[1] as f32]),
                );
                if iou >= threshold {
                    candidates.push((expected_index, predicted_index, iou));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut used_expected = Vec::new();
    let mut used_predicted = Vec::new();
    candidates
        .into_iter()
        .filter(|(expected_index, predicted_index, _)| {
            if used_expected.contains(expected_index) || used_predicted.contains(predicted_index) {
                false
            } else {
                used_expected.push(*expected_index);
                used_predicted.push(*predicted_index);
                true
            }
        })
        .collect()
}

pub fn polygon_iou(a: [[f32; 2]; 4], b: [[f32; 2]; 4]) -> f32 {
    let area_a = polygon_area(&a);
    let area_b = polygon_area(&b);
    let intersection = polygon_area(&clip_convex_polygon(a.to_vec(), &b));
    let union = area_a + area_b - intersection;
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn polygon_area(points: &[[f32; 2]]) -> f32 {
    if points.len() < 3 {
        return 0.0;
    }
    let area = points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| a[0] * b[1] - b[0] * a[1])
        .sum::<f32>()
        * 0.5;
    area.abs()
}

fn clip_convex_polygon(mut subject: Vec<[f32; 2]>, clip: &[[f32; 2]; 4]) -> Vec<[f32; 2]> {
    if subject.is_empty() {
        return subject;
    }
    let orientation = signed_area(clip).signum();
    for edge in clip.iter().zip(clip.iter().cycle().skip(1)).take(4) {
        let (start, end) = (*edge.0, *edge.1);
        let previous = subject;
        subject = Vec::new();
        for (current, next) in previous
            .iter()
            .zip(previous.iter().cycle().skip(1))
            .take(previous.len())
        {
            let current_inside = cross(start, end, *current) * orientation >= 0.0;
            let next_inside = cross(start, end, *next) * orientation >= 0.0;
            if current_inside != next_inside {
                subject.push(line_intersection(*current, *next, start, end));
            }
            if next_inside {
                subject.push(*next);
            }
        }
        if subject.is_empty() {
            break;
        }
    }
    subject
}

fn signed_area(points: &[[f32; 2]; 4]) -> f32 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(4)
        .map(|(a, b)| a[0] * b[1] - b[0] * a[1])
        .sum::<f32>()
        * 0.5
}

fn cross(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn line_intersection(a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) -> [f32; 2] {
    let r = [b[0] - a[0], b[1] - a[1]];
    let s = [d[0] - c[0], d[1] - c[1]];
    let denominator = r[0] * s[1] - r[1] * s[0];
    if denominator.abs() < f32::EPSILON {
        return b;
    }
    let q = [c[0] - a[0], c[1] - a[1]];
    let t = (q[0] * s[1] - q[1] * s[0]) / denominator;
    [a[0] + t * r[0], a[1] + t * r[1]]
}

#[cfg(test)]
mod tests {
    use super::{
        EvaluationReport, EvaluationSummary, character_error_rate, evaluate_case, polygon_iou,
    };

    #[test]
    fn cer_handles_unicode_insertions_and_empty_reference() {
        assert_eq!(character_error_rate("中文", "中X文"), 0.5);
        assert_eq!(character_error_rate("", "x"), 1.0);
    }

    #[test]
    fn evaluation_reports_detection_metrics() {
        let polygon = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let predicted = vec![crate::OcrJsonItem {
            box_: Some(polygon.map(|p| [p[0] as f64, p[1] as f64])),
            txt: "ok".into(),
            score: 0.9,
        }];
        let metrics = evaluate_case("ok", &[polygon], &predicted, 0.5);
        assert_eq!(metrics.cer, 0.0);
        assert_eq!(metrics.detection_recall, Some(1.0));
        assert_eq!(polygon_iou(polygon, polygon), 1.0);
    }

    #[test]
    fn summary_preserves_cases_and_averages_metrics() {
        let report = EvaluationReport {
            image: "fixture.png".into(),
            metrics: evaluate_case("ok", &[], &[], 0.5),
        };
        let summary = EvaluationSummary::from_cases(vec![report]);
        assert_eq!(summary.cases[0].image, "fixture.png");
        assert_eq!(summary.mean_cer, 1.0);
        assert_eq!(summary.exact_match_rate, 0.0);
        assert!(summary.mean_detection_recall.is_none());
    }
}

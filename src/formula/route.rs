//! 页面级公式区域的路由策略。
//!
//! 这个模块只做**几何决策**，不加载任何模型，因此可以在没有 ONNX 的环境里
//! 完整测试误检、漏检与重叠场景：
//!
//! - **重叠**：同一个公式被检测成多个框（含嵌套）时，按分数保留一个；
//! - **误检**：低于面积占比下限、或超出图像边界的候选被拒绝；
//! - **漏检**：没有任何候选时返回空集合，页面 OCR 必须退化为普通文本行为。
//!
//! 路由结果决定哪些像素会在送入普通文本管线之前被抹白，从而真正跳过 CTC。

use crate::{
    api::{ImageSize, Polygon},
    error::{RapidOcrError, Result},
};

/// 一个候选公式区域。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FormulaCandidate {
    pub polygon: Polygon,
    pub score: f32,
    /// 是否由检测模型给出（`false` 表示调用方显式声明）。
    pub detected: bool,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn from_polygon(polygon: Polygon) -> Self {
        let mut x0 = f32::INFINITY;
        let mut y0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        for [x, y] in polygon.points {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Self { x0, y0, x1, y1 }
    }

    fn width(self) -> f32 {
        (self.x1 - self.x0).max(0.0)
    }

    fn height(self) -> f32 {
        (self.y1 - self.y0).max(0.0)
    }

    fn area(self) -> f32 {
        self.width() * self.height()
    }

    fn intersection(self, other: Self) -> f32 {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        (x1 - x0).max(0.0) * (y1 - y0).max(0.0)
    }

    fn iou(self, other: Self) -> f32 {
        let intersection = self.intersection(other);
        let union = self.area() + other.area() - intersection;
        if union <= 0.0 {
            0.0
        } else {
            intersection / union
        }
    }

    /// `self` 被 `other` 覆盖的面积比例。
    fn covered_ratio(self, other: Self) -> f32 {
        let area = self.area();
        if area <= 0.0 {
            return 0.0;
        }
        self.intersection(other) / area
    }
}

/// 把候选裁剪到图像范围内；完全越界时返回 `None`。
fn clamp_polygon(polygon: Polygon, size: ImageSize) -> Option<Polygon> {
    let rect = Rect::from_polygon(polygon);
    if !rect.x0.is_finite() || !rect.y0.is_finite() || !rect.x1.is_finite() || !rect.y1.is_finite()
    {
        return None;
    }
    let x0 = rect.x0.max(0.0).min(size.width as f32);
    let y0 = rect.y0.max(0.0).min(size.height as f32);
    let x1 = rect.x1.max(0.0).min(size.width as f32);
    let y1 = rect.y1.max(0.0).min(size.height as f32);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        return None;
    }
    Some(Polygon {
        points: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
    })
}

/// 解析页面公式区域：过滤误检、消解重叠、限制数量。
///
/// `min_area_ratio` 是相对整页面积的最小占比，用于丢弃窄条/噪点误检；
/// `iou_threshold` 用于重叠消解（同 Ultralytics 的 NMS 语义）；
/// 显式声明的区域（`detected == false`）不参与面积过滤，因为它们由调用方直接给出。
pub fn resolve_formula_regions(
    candidates: &[FormulaCandidate],
    size: ImageSize,
    min_area_ratio: f32,
    iou_threshold: f32,
    max_regions: usize,
) -> Vec<FormulaCandidate> {
    let page_area = (size.width as f32) * (size.height as f32);
    let min_area = page_area * min_area_ratio.clamp(0.0, 1.0);

    let mut accepted: Vec<(FormulaCandidate, Rect)> = Vec::new();
    for candidate in candidates {
        let Some(polygon) = clamp_polygon(candidate.polygon, size) else {
            continue;
        };
        let rect = Rect::from_polygon(polygon);
        if !rect.area().is_finite() || rect.area() <= 0.0 {
            continue;
        }
        if candidate.detected && rect.area() < min_area {
            continue;
        }
        accepted.push((
            FormulaCandidate {
                polygon,
                score: candidate.score,
                detected: candidate.detected,
            },
            rect,
        ));
    }

    // 分数降序；同分时按面积降序，保证结果与输入顺序无关。
    accepted.sort_by(|a, b| {
        b.0.score
            .total_cmp(&a.0.score)
            .then_with(|| b.1.area().total_cmp(&a.1.area()))
            .then_with(|| a.1.x0.total_cmp(&b.1.x0))
            .then_with(|| a.1.y0.total_cmp(&b.1.y0))
    });

    let mut kept: Vec<(FormulaCandidate, Rect)> = Vec::new();
    for (candidate, rect) in accepted {
        let overlapping = kept.iter().any(|(_, other)| {
            rect.iou(*other) >= iou_threshold
                // 嵌套框：内层框被已保留的外层框高度覆盖时也视为重叠。
                || rect.covered_ratio(*other) >= 0.9
        });
        if !overlapping {
            kept.push((candidate, rect));
        }
        if kept.len() >= max_regions {
            break;
        }
    }

    kept.into_iter().map(|(candidate, _)| candidate).collect()
}

/// 抹白公式区域后的图像（RGB8），用于在送入普通文本管线**之前**移除公式像素。
///
/// 这是“公式区域跳过普通 CTC”的实现方式：公式像素根本不进入检测/识别，
/// 因此不会产生 CTC 文本，也不需要事后丢弃结果。
pub fn whiten_regions(
    image: &image::DynamicImage,
    regions: &[FormulaCandidate],
) -> Result<crate::config::RecImage> {
    use image::GenericImageView;

    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(RapidOcrError::InvalidInput(
            "formula routing requires a non-empty image".to_string(),
        ));
    }
    let mut rgb = image.to_rgb8();
    for (index, candidate) in regions.iter().enumerate() {
        let points: Vec<imageproc::point::Point<i32>> = candidate
            .polygon
            .points
            .iter()
            .map(|[x, y]| {
                imageproc::point::Point::new(
                    x.round().clamp(0.0, width as f32) as i32,
                    y.round().clamp(0.0, height as f32) as i32,
                )
            })
            .collect();
        let _ = index;
        imageproc::drawing::draw_polygon_mut(&mut rgb, &points, image::Rgb([255, 255, 255]));
    }
    let (width, height) = rgb.dimensions();
    crate::config::RecImage::from_rgb_u8(width as usize, height as usize, rgb.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polygon(x0: f32, y0: f32, x1: f32, y1: f32) -> Polygon {
        Polygon {
            points: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
        }
    }

    fn size() -> ImageSize {
        ImageSize {
            width: 100,
            height: 100,
        }
    }

    #[test]
    fn no_candidates_means_no_formula_regions() {
        // 漏检：检测模型没有给出任何候选时，页面必须退化为普通文本行为。
        let resolved = resolve_formula_regions(&[], size(), 0.0, 0.7, 64);
        assert!(resolved.is_empty());
    }

    #[test]
    fn nested_and_duplicate_detections_collapse_to_one() {
        // 重叠：同一次检测产生嵌套框与近似重复框，只保留分数最高的一个。
        let candidates = vec![
            FormulaCandidate {
                polygon: polygon(10.0, 10.0, 60.0, 40.0),
                score: 0.9,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(12.0, 12.0, 58.0, 38.0),
                score: 0.8,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(11.0, 11.0, 59.0, 39.0),
                score: 0.95,
                detected: true,
            },
        ];
        let resolved = resolve_formula_regions(&candidates, size(), 0.0, 0.7, 64);
        assert_eq!(resolved.len(), 1, "overlapping detections must collapse");
        assert!((resolved[0].score - 0.95).abs() < 1e-6);
    }

    #[test]
    fn distant_detections_are_all_kept() {
        let candidates = vec![
            FormulaCandidate {
                polygon: polygon(0.0, 0.0, 20.0, 10.0),
                score: 0.9,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(40.0, 60.0, 80.0, 90.0),
                score: 0.85,
                detected: true,
            },
        ];
        let resolved = resolve_formula_regions(&candidates, size(), 0.0, 0.7, 64);
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn tiny_and_out_of_bounds_detections_are_rejected() {
        // 误检：极窄条与完全越界的候选都不能进入公式区域。
        let candidates = vec![
            FormulaCandidate {
                polygon: polygon(10.0, 10.0, 10.4, 40.0),
                score: 0.99,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(200.0, 200.0, 260.0, 240.0),
                score: 0.99,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(10.0, 10.0, 40.0, 30.0),
                score: 0.5,
                detected: true,
            },
        ];
        let resolved = resolve_formula_regions(&candidates, size(), 0.0, 0.7, 64);
        assert_eq!(resolved.len(), 1);
        assert!((resolved[0].score - 0.5).abs() < 1e-6);
    }

    #[test]
    fn min_area_ratio_rejects_small_detections() {
        let candidates = vec![
            FormulaCandidate {
                polygon: polygon(0.0, 0.0, 5.0, 5.0),
                score: 0.99,
                detected: true,
            },
            FormulaCandidate {
                polygon: polygon(0.0, 0.0, 40.0, 40.0),
                score: 0.9,
                detected: true,
            },
        ];
        let resolved = resolve_formula_regions(&candidates, size(), 0.05, 0.7, 64);
        assert_eq!(resolved.len(), 1);
        assert!((resolved[0].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn explicit_regions_bypass_area_filter_but_are_clamped() {
        let candidates = vec![FormulaCandidate {
            polygon: polygon(-10.0, -10.0, 20.0, 20.0),
            score: 1.0,
            detected: false,
        }];
        let resolved = resolve_formula_regions(&candidates, size(), 0.5, 0.7, 64);
        assert_eq!(
            resolved.len(),
            1,
            "explicit regions must not be area-filtered"
        );
        assert_eq!(resolved[0].polygon.points[0], [0.0, 0.0]);
        assert_eq!(resolved[0].polygon.points[2], [20.0, 20.0]);
    }

    #[test]
    fn resolution_is_order_independent_and_capped() {
        let mut candidates: Vec<FormulaCandidate> = (0..20)
            .map(|index| FormulaCandidate {
                polygon: polygon(
                    (index % 4) as f32 * 25.0,
                    (index / 4) as f32 * 20.0,
                    (index % 4) as f32 * 25.0 + 20.0,
                    (index / 4) as f32 * 20.0 + 12.0,
                ),
                score: 0.5 + index as f32 * 0.01,
                detected: true,
            })
            .collect();
        let forward = resolve_formula_regions(&candidates, size(), 0.0, 0.7, 3);
        candidates.reverse();
        let backward = resolve_formula_regions(&candidates, size(), 0.0, 0.7, 3);
        assert_eq!(forward.len(), 3, "max_regions must cap the result");
        let forward_scores: Vec<f32> = forward.iter().map(|c| c.score).collect();
        let backward_scores: Vec<f32> = backward.iter().map(|c| c.score).collect();
        assert_eq!(
            forward_scores, backward_scores,
            "resolution must not depend on input order"
        );
    }

    #[test]
    fn whitening_replaces_formula_pixels_and_keeps_image_size() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            30,
            image::Rgb([10, 20, 30]),
        ));
        let regions = vec![FormulaCandidate {
            polygon: polygon(5.0, 5.0, 20.0, 20.0),
            score: 0.9,
            detected: true,
        }];
        let whitened = whiten_regions(&image, &regions).expect("whitening");
        assert_eq!(whitened.width(), 40);
        assert_eq!(whitened.height(), 30);
        let bytes = whitened.as_bytes();
        // (10, 10) 在区域内 -> 白； (30, 25) 在区域外 -> 原色。
        let inside = (10 * 40 + 10) * 3;
        assert_eq!(&bytes[inside..inside + 3], &[255, 255, 255]);
        let outside = (25 * 40 + 30) * 3;
        assert_eq!(&bytes[outside..outside + 3], &[10, 20, 30]);
    }

    #[test]
    fn whitening_rejects_empty_images() {
        let empty = image::DynamicImage::ImageRgb8(image::RgbImage::new(0, 0));
        assert!(whiten_regions(&empty, &[]).is_err());
    }
}

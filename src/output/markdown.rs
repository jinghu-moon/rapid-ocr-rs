#![allow(dead_code)]

use crate::{OcrOutput, Quad, TextOrder, error::Result};

#[derive(Debug, Clone, Copy)]
struct BoxProps {
    top: f32,
    bottom: f32,
    left: f32,
    height: f32,
    center_y: f32,
}

impl BoxProps {
    fn merge(self, other: Self) -> Self {
        let top = self.top.min(other.top);
        let bottom = self.bottom.max(other.bottom);
        Self {
            top,
            bottom,
            left: self.left.min(other.left),
            height: bottom - top,
            center_y: (top + bottom) * 0.5,
        }
    }
}

pub fn to_markdown(boxes: &[Quad], txts: &[String]) -> Result<String> {
    if boxes.len() != txts.len() {
        return Err(crate::error::RapidOcrError::InvalidInput(format!(
            "markdown output length mismatch: boxes={}, txts={}",
            boxes.len(),
            txts.len()
        )));
    }
    if boxes.is_empty() {
        return Ok("No text detected.".to_string());
    }

    let mut combined: Vec<(Quad, String)> = (0..boxes.len())
        .map(|i| (boxes[i], txts[i].clone()))
        .collect();
    combined.sort_by(|(a_box, _), (b_box, _)| {
        let a = get_box_properties(a_box);
        let b = get_box_properties(b_box);
        a.top
            .partial_cmp(&b.top)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.left
                    .partial_cmp(&b.left)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    let mut output_lines: Vec<String> = Vec::new();
    let mut current_line_parts: Vec<String> = vec![combined[0].1.clone()];
    let mut prev_props = get_box_properties(&combined[0].0);

    for (box_, text) in combined.iter().skip(1) {
        let current_props = get_box_properties(box_);

        let min_height = current_props.height.min(prev_props.height);
        let centers_are_close =
            (current_props.center_y - prev_props.center_y).abs() < (min_height * 0.5);

        let overlap_top = prev_props.top.max(current_props.top);
        let overlap_bottom = prev_props.bottom.min(current_props.bottom);
        let has_vertical_overlap = overlap_bottom > overlap_top;

        if centers_are_close || has_vertical_overlap {
            current_line_parts.push("   ".to_string());
            current_line_parts.push(text.clone());
        } else {
            output_lines.push(current_line_parts.join(""));

            let vertical_gap = current_props.top - prev_props.bottom;
            if vertical_gap > prev_props.height * 0.7 {
                output_lines.push(String::new());
            }

            current_line_parts = vec![text.clone()];
        }

        prev_props = current_props;
    }

    output_lines.push(current_line_parts.join(""));
    Ok(output_lines.join("\n"))
}

pub fn to_markdown_texts(txts: &[String]) -> String {
    if txts.is_empty() {
        return "No text detected.".to_string();
    }
    txts.join("\n")
}

pub fn to_output_markdown(output: &OcrOutput) -> String {
    for region in &output.regions {
        if region.recognition.is_some() && region.polygon.is_none() {
            return output.plain_text(TextOrder::Reading);
        }
    }

    let mut blocks: Vec<String> = Vec::new();
    let mut previous_props: Option<BoxProps> = None;

    for group in output.reading_order_groups() {
        let mut parts = Vec::new();
        let mut group_props: Option<BoxProps> = None;

        for id in group {
            let Some(region) = output.regions.get(id) else {
                continue;
            };
            let Some(recognition) = &region.recognition else {
                continue;
            };
            let Some(polygon) = region.polygon else {
                continue;
            };
            parts.push(recognition.text.clone());
            let props = get_box_properties(&polygon.points);
            group_props = Some(match group_props {
                Some(current) => current.merge(props),
                None => props,
            });
        }

        if parts.is_empty() {
            continue;
        }
        if let (Some(previous), Some(current)) = (previous_props, group_props) {
            let min_height = previous.height.min(current.height);
            let vertical_gap = current.top - previous.bottom;
            if vertical_gap > min_height * 0.7 {
                blocks.push(String::new());
            }
        }
        blocks.push(parts.join("   "));
        previous_props = group_props;
    }

    if blocks.is_empty() {
        output.plain_text(TextOrder::Reading)
    } else {
        blocks.join("\n")
    }
}

fn get_box_properties(box_: &Quad) -> BoxProps {
    let mut top = f32::INFINITY;
    let mut bottom = f32::NEG_INFINITY;
    let mut left = f32::INFINITY;
    for point in box_ {
        top = top.min(point[1]);
        bottom = bottom.max(point[1]);
        left = left.min(point[0]);
    }
    let height = bottom - top;
    BoxProps {
        top,
        bottom,
        left,
        height,
        center_y: top + height / 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::to_markdown;
    use crate::{
        CoordinateSpace, EngineInfo, GenericProviderPreference as ProviderPreference, ImageInfo,
        ImageSize, InputTimings, OcrOutput, OcrRegion, OcrTimings, Polygon, ProviderInfo,
        ProviderResolutionInfo, RecognitionOutcome, RegionSource, ResolvedProvider, StageReport,
        StageReports, StageState,
    };

    #[test]
    fn markdown_empty_message() {
        assert_eq!(
            to_markdown(&[], &[]).expect("empty should be valid"),
            "No text detected."
        );
    }

    fn text_region(id: usize, points: [[f32; 2]; 4], text: &str) -> OcrRegion {
        OcrRegion {
            source: RegionSource::Detected { detector_index: id },
            polygon: Some(Polygon { points }),
            detection: None,
            classification: None,
            recognition: Some(RecognitionOutcome {
                text: text.to_string(),
                score: 0.99,
                words: None,
            }),
        }
    }

    fn output_with_regions(regions: Vec<OcrRegion>) -> OcrOutput {
        let resolution = ProviderResolutionInfo {
            requested: ProviderPreference::Cpu,
            resolved: ResolvedProvider::Cpu,
            fallback_to_cpu: false,
        };
        OcrOutput {
            schema_version: 1,
            image: ImageInfo {
                original_size: ImageSize {
                    width: 200,
                    height: 100,
                },
                processed_size: ImageSize {
                    width: 200,
                    height: 100,
                },
                coordinate_space: CoordinateSpace::Image,
            },
            stages: StageReports {
                input: InputTimings::default(),
                detector: StageReport {
                    state: StageState::Completed {
                        items: regions.len(),
                    },
                    timing: None,
                },
                classifier: StageReport {
                    state: StageState::Disabled,
                    timing: None,
                },
                recognizer: StageReport {
                    state: StageState::Completed {
                        items: regions.len(),
                    },
                    timing: None,
                },
            },
            regions,
            timings: OcrTimings::default(),
            engine: EngineInfo {
                model_id: "test".to_string(),
                provider: ProviderInfo {
                    detector: resolution,
                    classifier: None,
                    recognizer: resolution,
                },
            },
        }
    }

    #[derive(serde::Deserialize)]
    struct RealLayoutFixture {
        boxes: Vec<RealLayoutBox>,
        expected: Vec<usize>,
    }

    #[derive(serde::Deserialize)]
    struct RealLayoutBox {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        text: String,
    }

    #[test]
    fn output_markdown_uses_real_10_columns_order() {
        let fixture: RealLayoutFixture = serde_json::from_str(include_str!(
            "../../tests/fixtures/reading_order_10_columns.json"
        ))
        .expect("real layout fixture should parse");
        let regions = fixture
            .boxes
            .iter()
            .enumerate()
            .map(|(id, b)| {
                text_region(
                    id,
                    [[b.x0, b.y0], [b.x1, b.y0], [b.x1, b.y1], [b.x0, b.y1]],
                    &b.text,
                )
            })
            .collect();
        let output = output_with_regions(regions);
        let markdown = super::to_output_markdown(&output);
        let lines: Vec<&str> = markdown
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let expected: Vec<&str> = fixture
            .expected
            .iter()
            .map(|index| fixture.boxes[*index].text.as_str())
            .collect();
        assert_eq!(lines, expected);
    }

    #[test]
    fn output_markdown_uses_reading_order_for_columns() {
        let output = output_with_regions(vec![
            text_region(
                0,
                [[10.0, 10.0], [60.0, 10.0], [60.0, 30.0], [10.0, 30.0]],
                "L1",
            ),
            text_region(
                1,
                [[10.0, 40.0], [60.0, 40.0], [60.0, 60.0], [10.0, 60.0]],
                "L2",
            ),
            text_region(
                2,
                [[100.0, 10.0], [150.0, 10.0], [150.0, 30.0], [100.0, 30.0]],
                "R1",
            ),
            text_region(
                3,
                [[100.0, 40.0], [150.0, 40.0], [150.0, 60.0], [100.0, 60.0]],
                "R2",
            ),
        ]);

        assert_eq!(super::to_output_markdown(&output), "L1\nL2\nR1\nR2");
    }
}

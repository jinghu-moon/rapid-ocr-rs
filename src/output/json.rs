use serde::Serialize;

use crate::{OcrOutput, Quad, error::Result};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OcrJsonItem {
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub box_: Option<[[f64; 2]; 4]>,
    pub txt: String,
    pub score: f64,
    /// 区域类型：`text` 或 `formula`。
    pub kind: &'static str,
    /// 公式区域才有的 LaTeX；文本区域为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eos_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[allow(dead_code)]
pub fn to_json_items(
    boxes: Option<&[Quad]>,
    txts: &[String],
    scores: &[f32],
) -> Result<Vec<OcrJsonItem>> {
    if txts.len() != scores.len() {
        return Err(crate::error::RapidOcrError::InvalidInput(format!(
            "json output length mismatch: txts={}, scores={}",
            txts.len(),
            scores.len()
        )));
    }

    if let Some(boxes) = boxes
        && boxes.len() != txts.len()
    {
        return Err(crate::error::RapidOcrError::InvalidInput(format!(
            "json output length mismatch: boxes={}, txts={}",
            boxes.len(),
            txts.len()
        )));
    }

    let mut out = Vec::with_capacity(txts.len());
    for i in 0..txts.len() {
        let out_box = boxes.map(|all| {
            let src_box = all[i];
            let mut out_box = [[0.0_f64; 2]; 4];
            for (dst, src) in out_box.iter_mut().zip(src_box.iter()) {
                dst[0] = src[0] as f64;
                dst[1] = src[1] as f64;
            }
            out_box
        });
        out.push(OcrJsonItem {
            box_: out_box,
            txt: txts[i].clone(),
            score: scores[i] as f64,
            kind: "text",
            latex: None,
            eos_index: None,
            truncated: None,
        });
    }
    Ok(out)
}

fn polygon_box(region: &crate::OcrRegion) -> Option<[[f64; 2]; 4]> {
    region.polygon.map(|polygon| {
        polygon
            .points
            .map(|point| [point[0] as f64, point[1] as f64])
    })
}

/// 文本与公式区域按 region 顺序统一为输出项。
///
/// 公式区域的 `txt` 就是 LaTeX：它是该区域唯一的文本表示，因此普通文本通道
/// 不会丢失公式内容，也不会把 LaTeX 伪装成 CTC 文本。
pub fn to_output_items(output: &OcrOutput) -> Vec<OcrJsonItem> {
    output
        .regions
        .iter()
        .filter_map(|region| {
            if let Some(formula) = region.formula.as_ref() {
                return Some(OcrJsonItem {
                    box_: polygon_box(region),
                    txt: formula.latex.clone(),
                    score: region
                        .detection
                        .as_ref()
                        .map(|detection| detection.score as f64)
                        .unwrap_or(0.0),
                    kind: "formula",
                    latex: Some(formula.latex.clone()),
                    eos_index: formula.eos_index,
                    truncated: Some(formula.truncated),
                });
            }
            let recognition = region.recognition.as_ref()?;
            Some(OcrJsonItem {
                box_: polygon_box(region),
                txt: recognition.text.clone(),
                score: recognition.score as f64,
                kind: "text",
                latex: None,
                eos_index: None,
                truncated: None,
            })
        })
        .collect()
}

pub fn to_output_json(output: &OcrOutput) -> Result<Value> {
    let mut value = serde_json::to_value(output)
        .map_err(|error| crate::error::RapidOcrError::InvalidInput(error.to_string()))?;
    if let Value::Object(object) = &mut value {
        object.insert(
            "text".into(),
            Value::String(output.plain_text(crate::TextOrder::Reading)),
        );
        object.insert(
            "items".into(),
            serde_json::to_value(to_output_items(output))
                .map_err(|error| crate::error::RapidOcrError::InvalidInput(error.to_string()))?,
        );
        // 公式 LaTeX 单独给出，避免调用方必须从 items 里再筛一遍 kind。
        object.insert(
            "formulas".into(),
            serde_json::to_value(
                output
                    .formula_latex(crate::TextOrder::Reading)
                    .into_iter()
                    .map(|(region_id, latex)| {
                        serde_json::json!({ "region": region_id, "latex": latex })
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| crate::error::RapidOcrError::InvalidInput(error.to_string()))?,
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{to_json_items, to_output_items, to_output_json};
    use crate::{
        CoordinateSpace, EngineInfo, FormulaOutcome,
        GenericProviderPreference as ProviderPreference, ImageInfo, InputTimings, OcrOutput,
        OcrRegion, OcrTimings, Polygon, ProviderInfo, ProviderResolutionInfo, RecognitionOutcome,
        RegionKind, RegionSource, ResolvedProvider, StageReport, StageReports, StageState,
    };

    #[test]
    fn json_items_none_for_empty_inputs() {
        assert_eq!(
            to_json_items(Some(&[]), &[], &[]).expect("empty should be valid"),
            Vec::new()
        );
    }

    #[test]
    fn formula_region_uses_latex_and_stays_out_of_plain_text() {
        let output = output_with(vec![
            OcrRegion::text(
                RegionSource::Detected { detector_index: 0 },
                Some(Polygon {
                    points: [[1.0, 2.0], [3.0, 2.0], [3.0, 4.0], [1.0, 4.0]],
                }),
                None,
                None,
                Some(RecognitionOutcome {
                    text: "ok".into(),
                    score: 0.9,
                    words: None,
                }),
            ),
            OcrRegion {
                source: RegionSource::Detected { detector_index: 0 },
                kind: RegionKind::Formula,
                polygon: Some(Polygon {
                    points: [[1.0, 6.0], [5.0, 6.0], [5.0, 9.0], [1.0, 9.0]],
                }),
                detection: Some(crate::DetectionOutcome { score: 0.71 }),
                classification: None,
                recognition: None,
                formula: Some(FormulaOutcome {
                    latex: "\\frac{a}{b}".into(),
                    eos_index: Some(3),
                    truncated: false,
                    model_id: "pp_formulanet_plus_m".into(),
                    token_ids: None,
                }),
            },
        ]);

        let items = to_output_items(&output);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, "text");
        assert_eq!(items[0].latex, None);
        assert_eq!(items[1].kind, "formula");
        assert_eq!(items[1].txt, "\\frac{a}{b}");
        assert_eq!(items[1].latex.as_deref(), Some("\\frac{a}{b}"));
        assert_eq!(items[1].truncated, Some(false));

        let document = to_output_json(&output).expect("json document");
        assert_eq!(document["text"], "ok");
        assert_eq!(document["formulas"][0]["latex"], "\\frac{a}{b}");
        assert_eq!(document["regions"][1]["kind"], "formula");
        assert_eq!(document["regions"][1]["formula"]["eos_index"], 3);
        assert_eq!(output.len(), 2, "formula regions are recognition results");
        assert_eq!(output.text_len(), 1);
        assert!(!output.is_empty(), "a formula-only page is not empty");
    }

    #[test]
    fn formula_only_output_is_not_empty() {
        let output = output_with(vec![OcrRegion {
            source: RegionSource::Input,
            kind: RegionKind::Formula,
            polygon: Some(Polygon {
                points: [[1.0, 1.0], [9.0, 1.0], [9.0, 4.0], [1.0, 4.0]],
            }),
            detection: None,
            classification: None,
            recognition: None,
            formula: Some(FormulaOutcome {
                latex: "x".into(),
                eos_index: Some(1),
                truncated: false,
                model_id: "m".into(),
                token_ids: None,
            }),
        }]);
        output
            .validate()
            .expect("formula-only output must validate");
        assert_eq!(output.len(), 1);
        assert_eq!(output.text_len(), 0);
        assert!(!output.is_empty());
        assert!(output.plain_text(crate::TextOrder::Reading).is_empty());
    }

    #[test]
    fn invalid_region_kinds_are_rejected_by_validate() {
        let mut output = output_with(vec![OcrRegion {
            source: RegionSource::Input,
            kind: RegionKind::Formula,
            polygon: None,
            detection: None,
            classification: None,
            recognition: Some(RecognitionOutcome {
                text: "leaked".into(),
                score: 1.0,
                words: None,
            }),
            formula: None,
        }]);
        assert!(
            output.validate().is_err(),
            "formula region without formula outcome must be rejected"
        );
        output.regions[0].formula = Some(FormulaOutcome {
            latex: "x".into(),
            eos_index: None,
            truncated: true,
            model_id: "m".into(),
            token_ids: None,
        });
        assert!(
            output.validate().is_err(),
            "formula region carrying CTC text must be rejected"
        );
    }

    fn output_with(regions: Vec<OcrRegion>) -> OcrOutput {
        let provider = ProviderResolutionInfo {
            requested: ProviderPreference::Cpu,
            selected_ep: ResolvedProvider::Cpu,
            fallback_to_cpu: false,
        };
        OcrOutput {
            schema_version: 1,
            image: ImageInfo {
                original_size: crate::ImageSize {
                    width: 10,
                    height: 10,
                },
                processed_size: crate::ImageSize {
                    width: 10,
                    height: 10,
                },
                coordinate_space: CoordinateSpace::Image,
            },
            stages: StageReports {
                input: InputTimings::default(),
                detector: StageReport {
                    state: StageState::Completed { items: 1 },
                    timing: None,
                },
                classifier: StageReport {
                    state: StageState::Disabled,
                    timing: None,
                },
                recognizer: StageReport {
                    state: StageState::Completed { items: 1 },
                    timing: None,
                },
                formula: StageReport::default(),
            },
            regions,
            timings: OcrTimings::default(),
            engine: EngineInfo {
                model_id: "test".into(),
                provider: ProviderInfo {
                    detector: provider,
                    classifier: None,
                    recognizer: provider,
                },
            },
        }
    }
}

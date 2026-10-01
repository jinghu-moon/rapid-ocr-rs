use serde::Serialize;

use crate::{OcrOutput, Quad, error::Result};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OcrJsonItem {
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub box_: Option<[[f64; 2]; 4]>,
    pub txt: String,
    pub score: f64,
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
        });
    }
    Ok(out)
}

pub fn to_output_items(output: &OcrOutput) -> Vec<OcrJsonItem> {
    output
        .regions
        .iter()
        .filter_map(|region| {
            let recognition = region.recognition.as_ref()?;
            Some(OcrJsonItem {
                box_: region.polygon.map(|polygon| {
                    polygon
                        .points
                        .map(|point| [point[0] as f64, point[1] as f64])
                }),
                txt: recognition.text.clone(),
                score: recognition.score as f64,
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
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{to_json_items, to_output_items, to_output_json};
    use crate::{
        CoordinateSpace, EngineInfo, GenericProviderPreference as ProviderPreference, ImageInfo,
        InputTimings, OcrOutput, OcrRegion, OcrTimings, Polygon, ProviderInfo,
        ProviderResolutionInfo, RecognitionOutcome, RegionSource, ResolvedProvider, StageReport,
        StageReports, StageState,
    };

    #[test]
    fn json_items_none_for_empty_inputs() {
        assert_eq!(
            to_json_items(Some(&[]), &[], &[]).expect("empty should be valid"),
            Vec::new()
        );
    }

    #[test]
    fn output_items_preserve_region_polygon_and_text() {
        let provider = ProviderResolutionInfo {
            requested: ProviderPreference::Cpu,
            resolved: ResolvedProvider::Cpu,
            fallback_to_cpu: false,
        };
        let output = OcrOutput {
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
            },
            regions: vec![OcrRegion {
                source: RegionSource::Detected { detector_index: 0 },
                polygon: Some(Polygon {
                    points: [[1.0, 2.0], [3.0, 2.0], [3.0, 4.0], [1.0, 4.0]],
                }),
                detection: None,
                classification: None,
                recognition: Some(RecognitionOutcome {
                    text: "ok".into(),
                    score: 0.9,
                    words: None,
                }),
            }],
            timings: OcrTimings::default(),
            engine: EngineInfo {
                model_id: "test".into(),
                provider: ProviderInfo {
                    detector: provider,
                    classifier: None,
                    recognizer: provider,
                },
            },
        };
        let items = to_output_items(&output);
        assert_eq!(items[0].txt, "ok");
        assert_eq!(items[0].box_.expect("polygon")[0], [1.0, 2.0]);
        let document = to_output_json(&output).expect("json document");
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["text"], "ok");
    }
}

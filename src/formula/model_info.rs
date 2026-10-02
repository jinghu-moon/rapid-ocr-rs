//! 公式识别模型契约探针。
//!
//! 阶段 3 在共享 runtime 之上实现独立探针：读取 ONNX 签名与 `character` metadata，
//! 输出结构化 [`FormulaModelInfo`]。模型验证逻辑与推理执行分离——本模块只做
//! 契约验证，推理由 `runtime::session::OrtSession` 提供。

use std::path::Path;

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    runtime::session::{OrtSession, TensorSpec},
};

/// 公式模型签名探针结果。
#[derive(Debug, Clone)]
pub struct FormulaModelInfo {
    pub model_path: String,
    /// metadata `character` 原始 JSON（内嵌 `fast_tokenizer_file` / `tokenizer_config_file`）。
    pub character_metadata: String,
    pub input_name: String,
    pub input_element_type: &'static str,
    pub output_name: String,
    pub output_element_type: &'static str,
    pub input_dims: Vec<i64>,
}

/// 输入契约：单输入 `FLOAT [N, 1, 384, 384]`。
pub const FORMULA_INPUT_RANK: usize = 4;
pub const FORMULA_INPUT_SPATIAL: i64 = 384;
pub const FORMULA_INPUT_CHANNELS: i64 = 1;

/// 输出契约：单输出 `INT64 [N, L]`。
pub const FORMULA_OUTPUT_RANK: usize = 2;

impl FormulaModelInfo {
    /// 探针入口：加载模型、校验签名与 metadata，不执行推理。
    pub fn probe(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        if !model_path.is_file() {
            return Err(RapidOcrError::FileNotFound(model_path.to_path_buf()));
        }

        let session = OrtSession::open_unchecked(model_path, runtime_cfg)?;
        let io = session.probe_io()?;

        if io.inputs.len() != 1 {
            return Err(RapidOcrError::Config(format!(
                "formula model must expose exactly one input, got {} (model={})",
                io.inputs.len(),
                model_path.display()
            )));
        }
        if io.outputs.len() != 1 {
            return Err(RapidOcrError::Config(format!(
                "formula model must expose exactly one output, got {} (model={})",
                io.outputs.len(),
                model_path.display()
            )));
        }

        let input = &io.inputs[0];
        let output = &io.outputs[0];
        validate_input_spec(model_path, input)?;
        validate_output_spec(model_path, output)?;

        let character_metadata = match session.metadata_custom("character")? {
            Some(raw) => raw,
            None => {
                return Err(RapidOcrError::Config(format!(
                    "formula model is missing `character` metadata; refusing to fall back to the \
                     ordinary OCR dictionary (model={})",
                    model_path.display()
                )));
            }
        };

        // 确保 metadata 是合法 JSON（内嵌 tokenizer 在阶段 6 解析）。
        let _ = serde_json::from_str::<serde_json::Value>(&character_metadata).map_err(|e| {
            RapidOcrError::Config(format!(
                "formula model `character` metadata is not valid JSON (model={}): {e}",
                model_path.display()
            ))
        })?;

        fn element_type_str(t: ort::value::TensorElementType) -> &'static str {
            match t {
                ort::value::TensorElementType::Float32 => "FLOAT32",
                ort::value::TensorElementType::Int64 => "INT64",
                _ => "OTHER",
            }
        }
        Ok(Self {
            model_path: model_path.display().to_string(),
            input_name: input.name.clone(),
            input_element_type: element_type_str(input.element_type),
            input_dims: input.dims.clone(),
            output_name: output.name.clone(),
            output_element_type: element_type_str(output.element_type),
            character_metadata,
        })
    }
}

fn validate_input_spec(model_path: &Path, input: &TensorSpec) -> Result<()> {
    if input.rank != FORMULA_INPUT_RANK {
        return Err(RapidOcrError::Config(format!(
            "formula model input `{}` must be rank {FORMULA_INPUT_RANK}, got {} (model={})",
            input.name,
            input.rank,
            model_path.display()
        )));
    }
    if input.element_type != ort::value::TensorElementType::Float32 {
        return Err(RapidOcrError::Config(format!(
            "formula model input `{}` must be FLOAT32, got {:?} (model={})",
            input.name,
            input.element_type,
            model_path.display()
        )));
    }
    // 固定空间维：如果给出了具体值则必须为 384；动态维（<0）允许并记录。
    let channels = input.dims.get(1).copied().unwrap_or(-1);
    let height = input.dims.get(2).copied().unwrap_or(-1);
    let width = input.dims.get(3).copied().unwrap_or(-1);
    if channels >= 0 && channels != FORMULA_INPUT_CHANNELS {
        return Err(RapidOcrError::Config(format!(
            "formula model input `{}` must have {} channel, got {channels} (model={})",
            input.name,
            FORMULA_INPUT_CHANNELS,
            model_path.display()
        )));
    }
    if (height >= 0 && height != FORMULA_INPUT_SPATIAL)
        || (width >= 0 && width != FORMULA_INPUT_SPATIAL)
    {
        return Err(RapidOcrError::Config(format!(
            "formula model input `{}` must be {FORMULA_INPUT_SPATIAL}x{FORMULA_INPUT_SPATIAL}, \
             got [_, {channels}, {height}, {width}] (model={})",
            input.name,
            model_path.display()
        )));
    }
    Ok(())
}

fn validate_output_spec(model_path: &Path, output: &TensorSpec) -> Result<()> {
    if output.rank != FORMULA_OUTPUT_RANK {
        return Err(RapidOcrError::Config(format!(
            "formula model output `{}` must be rank {FORMULA_OUTPUT_RANK} (token sequence), \
             got {} (model={})",
            output.name,
            output.rank,
            model_path.display()
        )));
    }
    if output.element_type != ort::value::TensorElementType::Int64 {
        return Err(RapidOcrError::Config(format!(
            "formula model output `{}` must be INT64, got {:?} (model={})",
            output.name,
            output.element_type,
            model_path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const FIXTURES: &str =
        r"D:\100_Projects\110_Daily\SnapClip\crates\rapid-ocr-rs\tests\fixtures\formula-onnx";

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(FIXTURES).join(name)
    }

    fn rt() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    #[test]
    fn missing_model_returns_located_error() {
        let err =
            FormulaModelInfo::probe(&fixture("does_not_exist.onnx"), &rt()).expect_err("must fail");
        assert!(matches!(err, RapidOcrError::FileNotFound(_)));
    }

    #[test]
    fn rejects_input_rank() {
        let err = FormulaModelInfo::probe(&fixture("formula_input_rank3.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("rank"), "error: {err}");
    }

    #[test]
    fn rejects_input_dtype() {
        let err = FormulaModelInfo::probe(&fixture("formula_input_int64.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("FLOAT32"), "error: {err}");
    }

    #[test]
    fn rejects_input_spatial_dims() {
        let err = FormulaModelInfo::probe(&fixture("formula_input_512.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("384"), "error: {err}");
    }

    #[test]
    fn rejects_output_rank() {
        let err = FormulaModelInfo::probe(&fixture("formula_output_rank1.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("rank"), "error: {err}");
    }

    #[test]
    fn rejects_output_dtype() {
        let err = FormulaModelInfo::probe(&fixture("formula_output_f32.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("INT64"), "error: {err}");
    }

    #[test]
    fn rejects_multi_input() {
        let err = FormulaModelInfo::probe(&fixture("formula_multi_input.onnx"), &rt())
            .expect_err("must fail");
        assert!(
            err.to_string().contains("exactly one input"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_multi_output() {
        let err = FormulaModelInfo::probe(&fixture("formula_multi_output.onnx"), &rt())
            .expect_err("must fail");
        assert!(
            err.to_string().contains("exactly one output"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_missing_character_metadata() {
        let err = FormulaModelInfo::probe(&fixture("formula_no_metadata.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("character"), "error: {err}");
    }

    #[test]
    fn rejects_bad_character_metadata_json() {
        let err = FormulaModelInfo::probe(&fixture("formula_bad_metadata.onnx"), &rt())
            .expect_err("must fail");
        assert!(err.to_string().contains("valid JSON"), "error: {err}");
    }

    #[test]
    fn ok_model_probes_full_signature_and_metadata() {
        let info = FormulaModelInfo::probe(&fixture("formula_ok.onnx"), &rt())
            .expect("ok fixture should probe");
        assert_eq!(info.input_name, "x");
        assert_eq!(info.input_dims.len(), 4);
        assert_eq!(info.input_element_type, "FLOAT32");
        assert_eq!(info.output_name, "fetch_name_0");
        assert_eq!(info.output_element_type, "INT64");
        let json: serde_json::Value = serde_json::from_str(&info.character_metadata).unwrap();
        assert!(json.get("fast_tokenizer_file").is_some());
    }
}

#[cfg(test)]
mod real_model_tests {
    use std::path::{Path, PathBuf};

    use crate::{config::RuntimeConfig, formula::model_info::FormulaModelInfo};

    fn real_model() -> Option<PathBuf> {
        let env = std::env::var("RAPID_OCR_MODEL_ROOT")
            .or_else(|_| std::env::var("RAPID_OCR_FORMULA_MODEL_PATH"));
        if let Ok(p) = env {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Some(pb);
            }
            let candidate = pb.join("Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        // fallback: well-known absolute path
        let fallback = Path::new(
            r"D:\100_Projects\110_Daily\SnapClip\OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx",
        );
        if fallback.is_file() {
            Some(fallback.to_path_buf())
        } else {
            None
        }
    }

    #[test]
    fn real_model_probe_smoke() {
        let path = real_model().expect("real model not found; set RAPID_OCR_MODEL_ROOT");
        let info =
            FormulaModelInfo::probe(&path, &RuntimeConfig::default()).expect("real model probe");
        assert_eq!(info.input_name, "x");
        assert_eq!(info.input_element_type, "FLOAT32");
        assert_eq!(info.output_name, "fetch_name_0");
        assert_eq!(info.output_element_type, "INT64");
    }

    #[test]
    fn real_model_metadata_contains_tokenizer() {
        let path = real_model().expect("real model not found; set RAPID_OCR_MODEL_ROOT");
        let info =
            FormulaModelInfo::probe(&path, &RuntimeConfig::default()).expect("real model probe");
        let json: serde_json::Value = serde_json::from_str(&info.character_metadata)
            .expect("character metadata is not valid JSON");
        assert!(
            json.get("fast_tokenizer_file").is_some(),
            "character metadata must contain fast_tokenizer_file"
        );
        assert!(
            json.get("tokenizer_config_file").is_some(),
            "character metadata must contain tokenizer_config_file"
        );
        // verify vocab size
        if let Some(ft) = json.get("fast_tokenizer_file") {
            let vocab = ft.get("model").and_then(|m| m.get("vocab"));
            if let Some(v) = vocab {
                let count = v.as_object().map(|o| o.len()).unwrap_or(0);
                assert_eq!(count, 50000, "vocab size must be 50000, got {count}");
            }
        }
        // verify tokenizer config has expected special tokens
        if let Some(tc) = json.get("tokenizer_config_file") {
            assert_eq!(tc.get("bos_token").and_then(|v| v.as_str()), Some("<s>"));
            assert_eq!(tc.get("eos_token").and_then(|v| v.as_str()), Some("</s>"));
            assert_eq!(tc.get("pad_token").and_then(|v| v.as_str()), Some("<pad>"));
            assert_eq!(tc.get("unk_token").and_then(|v| v.as_str()), Some("<unk>"));
            assert!(
                tc.get("model_max_length")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    >= 512
            );
        }
    }

    #[test]
    fn real_model_zero_input_produces_int64_tokens() {
        let path = real_model().expect("real model not found; set RAPID_OCR_MODEL_ROOT");
        let rt = RuntimeConfig::default();
        let mut session =
            crate::runtime::session::OrtSession::open_unchecked(&path, &rt).expect("open session");
        use ndarray::Array4;
        let batch = 2usize;
        let input = Array4::<f32>::zeros((batch, 1, 384, 384));
        let output = session
            .run_i64_2d("x", input.view())
            .expect("run inference");
        assert_eq!(output.nrows(), batch, "output rows must equal batch size");
        assert!(
            output.ncols() >= 3,
            "output must have at least BOS+content+EOS tokens"
        );
        // first token of each batch should be BOS (0)
        for b in 0..batch {
            assert_eq!(output[[b, 0]], 0, "batch {b} first token must be BOS (0)");
        }
    }
}

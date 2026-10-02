//! 公式识别模型契约探针。
//!
//! 阶段 3 在共享 runtime 之上实现独立探针：读取 ONNX 签名与 `character` metadata，
//! 输出结构化 [`FormulaModelInfo`]。模型验证逻辑与推理执行分离——本模块只做
//! 契约验证，推理由 `runtime::session::OrtSession` 提供。

use std::path::Path;

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    formula::contract::validate_formula_contract,
    runtime::session::OrtSession,
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

impl FormulaModelInfo {
    /// 探针入口：加载模型、校验签名与 metadata，不执行推理。
    ///
    /// IO 契约校验复用 [`validate_formula_contract`]，与 `FormulaSession::new`
    /// 使用同一份实现，因此探针结论与运行时可接受性一致。
    pub fn probe(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        if !model_path.is_file() {
            return Err(RapidOcrError::FileNotFound(model_path.to_path_buf()));
        }

        let session = OrtSession::open_unchecked(model_path, runtime_cfg)?;
        let io = session.probe_io()?;
        validate_formula_contract(&io, model_path)?;
        let input = &io.inputs[0];
        let output = &io.outputs[0];

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        crate::test_support::fixture_dir("formula-onnx").join(name)
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
    use std::path::PathBuf;

    use crate::{config::RuntimeConfig, formula::model_info::FormulaModelInfo};

    /// 真实 594 MB 模型不在仓库内；缺失时必须 skip，不允许 panic 或回落到开发机路径。
    fn real_model() -> Option<PathBuf> {
        crate::test_support::formula_model_path()
    }

    #[test]
    fn real_model_probe_smoke() {
        let Some(path) = real_model() else {
            return;
        };
        let info =
            FormulaModelInfo::probe(&path, &RuntimeConfig::default()).expect("real model probe");
        assert_eq!(info.input_name, "x");
        assert_eq!(info.input_element_type, "FLOAT32");
        assert_eq!(info.output_name, "fetch_name_0");
        assert_eq!(info.output_element_type, "INT64");
    }

    #[test]
    fn real_model_metadata_contains_tokenizer() {
        let Some(path) = real_model() else {
            return;
        };
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
        let Some(path) = real_model() else {
            return;
        };
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

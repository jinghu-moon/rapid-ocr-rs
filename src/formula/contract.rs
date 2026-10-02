//! PP-FormulaNet_plus 模型 IO 契约的**唯一**实现。
//!
//! `FormulaModelInfo::probe`（公共探针）和 `FormulaSession::new`（运行时 session）
//! 必须共用同一份校验，否则会出现 “probe 成功、session 失败” 的契约不一致：
//! 调用方按探针结果准备输入 tensor，却在构造 session 时被拒绝。
//!
//! 契约事实：
//!
//! - 输入：单输入 `FLOAT32 [N, 1, 384, 384]`，只有 batch 维可以是动态值；
//! - 输出：单输出 `INT64 [N, L]`。
//!
//! 通道与空间维必须是固定值：公式预处理始终产出 `[N,1,384,384]`，接受动态
//! 空间维不会带来任何额外能力，却会让探针结论失去可操作性。

use std::path::Path;

use ort::value::TensorElementType;

use crate::{
    error::Result,
    runtime::contracts::{
        ModelIoProbe, require_fixed_dim, require_single_input, require_single_output,
        require_tensor,
    },
};

/// 输入契约：单输入 `FLOAT [N, 1, 384, 384]`。
pub const FORMULA_INPUT_RANK: usize = 4;
pub const FORMULA_INPUT_SPATIAL: i64 = 384;
pub const FORMULA_INPUT_CHANNELS: i64 = 1;

/// 输出契约：单输出 `INT64 [N, L]`。
pub const FORMULA_OUTPUT_RANK: usize = 2;

/// 通过校验的公式模型签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaContract {
    pub input_name: String,
    pub output_name: String,
}

/// 校验公式模型 IO 契约；探针与运行时 session 共用。
pub fn validate_formula_contract(
    probe: &ModelIoProbe,
    model_path: &Path,
) -> Result<FormulaContract> {
    let input = require_single_input(probe, model_path, "formula")?;
    require_tensor(
        input,
        "input",
        FORMULA_INPUT_RANK,
        TensorElementType::Float32,
        model_path,
    )?;
    require_fixed_dim(input, "input", 1, FORMULA_INPUT_CHANNELS, model_path)?;
    require_fixed_dim(input, "input", 2, FORMULA_INPUT_SPATIAL, model_path)?;
    require_fixed_dim(input, "input", 3, FORMULA_INPUT_SPATIAL, model_path)?;

    let output = require_single_output(probe, model_path, "formula")?;
    require_tensor(
        output,
        "output",
        FORMULA_OUTPUT_RANK,
        TensorElementType::Int64,
        model_path,
    )?;

    Ok(FormulaContract {
        input_name: input.name.clone(),
        output_name: output.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::runtime::session::OrtSession;
    use crate::{config::RuntimeConfig, formula::session::FormulaSession};

    fn fixture(name: &str) -> PathBuf {
        crate::test_support::fixture_dir("formula-onnx").join(name)
    }

    /// 公共探针与运行时 session 必须对同一模型给出**一致**的成功/失败结论。
    #[test]
    fn probe_and_session_agree_on_every_fixture() {
        for name in [
            "formula_ok.onnx",
            "formula_no_metadata.onnx",
            "formula_bad_metadata.onnx",
            "formula_multi_input.onnx",
            "formula_multi_output.onnx",
            "formula_input_int64.onnx",
            "formula_input_rank3.onnx",
            "formula_input_512.onnx",
            "formula_output_f32.onnx",
            "formula_output_rank1.onnx",
        ] {
            let path = fixture(name);
            let runtime = RuntimeConfig::default();
            let session = OrtSession::open_unchecked(&path, &runtime).expect("fixture must load");
            let probe = session.probe_io().expect("fixture must probe");
            let contract = validate_formula_contract(&probe, &path);

            // `FormulaSession::new` 在契约通过后可能因为 metadata 之外的运行原因失败，
            // 因此这里比较“契约层”的结论：契约失败必须同时让 session 失败。
            let session_result = FormulaSession::new(&path, &runtime);
            if let Err(error) = &contract {
                let session_error =
                    session_result.expect_err("contract failure must also fail the typed session");
                assert_eq!(
                    session_error.to_string(),
                    error.to_string(),
                    "probe/session contract messages must match for {name}"
                );
            }
        }
    }

    #[test]
    fn spatial_dims_must_be_fixed_at_384() {
        let path = fixture("formula_input_512.onnx");
        let runtime = RuntimeConfig::default();
        let session = OrtSession::open_unchecked(&path, &runtime).expect("fixture must load");
        let probe = session.probe_io().expect("fixture must probe");
        let error = validate_formula_contract(&probe, &path).expect_err("512 must be rejected");
        assert!(error.to_string().contains("384"), "error: {error}");
    }

    #[test]
    fn dynamic_spatial_dims_are_rejected_consistently() {
        use crate::runtime::contracts::TensorSpec;
        let probe = ModelIoProbe {
            inputs: vec![TensorSpec {
                name: "x".to_string(),
                rank: 4,
                dims: vec![-1, 1, -1, -1],
                element_type: TensorElementType::Float32,
            }],
            outputs: vec![TensorSpec {
                name: "fetch_name_0".to_string(),
                rank: 2,
                dims: vec![-1, -1],
                element_type: TensorElementType::Int64,
            }],
        };
        let error = validate_formula_contract(&probe, Path::new("dynamic.onnx"))
            .expect_err("dynamic spatial dims must be rejected");
        assert!(error.to_string().contains("384"), "error: {error}");
    }
}

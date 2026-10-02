//! PP-FormulaNet_plus typed ONNX session 契约。
//!
//! 共享 `runtime::session::OrtSession` 只暴露通用 ONNX 能力；公式 token 模型
//! 的 rank-2 `INT64` 输出和固定 `[N,1,384,384]` FLOAT 输入在这里固化。

use std::path::{Path, PathBuf};

use ndarray::{Array2, ArrayView4};

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    formula::contract::validate_formula_contract,
    runtime::{
        provider::{ProviderResolution, require_requested_provider},
        session::OrtSession,
    },
};
#[derive(Debug)]
pub struct FormulaSession {
    inner: OrtSession,
    input_name: String,
    output_name: String,
    model_path: PathBuf,
}

impl FormulaSession {
    pub fn new(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        if !model_path.is_file() {
            return Err(RapidOcrError::FileNotFound(model_path.to_path_buf()));
        }
        let inner = OrtSession::open_unchecked(model_path, runtime_cfg)?;
        // 公式域契约：请求了加速器却只拿到 CPU 回退时必须失败，不能静默降级。
        // 这里不依赖 `RuntimeConfig::fail_if_provider_unavailable`，因为那是进程级
        // 默认策略，而调用方无法从 API 返回值观察到发生了回退。
        require_requested_provider(inner.provider_resolution())?;
        let probe = inner.probe_io()?;
        let contract = validate_formula_contract(&probe, model_path)?;

        Ok(Self {
            inner,
            input_name: contract.input_name,
            output_name: contract.output_name,
            model_path: model_path.to_path_buf(),
        })
    }

    pub fn input_name(&self) -> &str {
        &self.input_name
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.inner.provider_resolution()
    }

    pub fn character_metadata(&self) -> Result<Option<String>> {
        self.inner.metadata_custom("character")
    }

    pub fn run(&mut self, input: ArrayView4<'_, f32>) -> Result<Array2<i64>> {
        let shape = input.shape();
        if shape[1] != 1 || shape[2] != 384 || shape[3] != 384 {
            return Err(RapidOcrError::InvalidInput(format!(
                "formula input must be [N,1,384,384], got {:?} (model={})",
                shape,
                self.model_path.display()
            )));
        }
        self.inner.run_i64_2d(&self.input_name, input)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ndarray::Array4;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/formula-onnx")
            .join(name)
    }

    fn rt() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    #[test]
    fn accepts_formula_contract_and_runs() {
        let mut session = FormulaSession::new(&fixture("formula_ok.onnx"), &rt())
            .expect("formula fixture should load");
        assert_eq!(session.input_name(), "x");
        assert_eq!(session.output_name(), "fetch_name_0");
        let input = Array4::<f32>::zeros((1, 1, 384, 384));
        let output = session
            .run(input.view())
            .expect("formula fixture should run");
        assert_eq!(output.nrows(), 1);
        assert!(output.ncols() >= 1);
    }

    #[test]
    fn rejects_input_rank() {
        let error = FormulaSession::new(&fixture("formula_input_rank3.onnx"), &rt())
            .expect_err("rank3 input must fail");
        assert!(error.to_string().contains("rank"), "error: {error}");
    }

    #[test]
    fn rejects_input_dtype() {
        let error = FormulaSession::new(&fixture("formula_input_int64.onnx"), &rt())
            .expect_err("int64 input must fail");
        assert!(error.to_string().contains("FLOAT32"), "error: {error}");
    }

    #[test]
    fn rejects_input_spatial_dims() {
        let error = FormulaSession::new(&fixture("formula_input_512.onnx"), &rt())
            .expect_err("512 input must fail");
        assert!(error.to_string().contains("384"), "error: {error}");
    }

    #[test]
    fn rejects_output_rank() {
        let error = FormulaSession::new(&fixture("formula_output_rank1.onnx"), &rt())
            .expect_err("rank1 output must fail");
        assert!(error.to_string().contains("rank"), "error: {error}");
    }

    #[test]
    fn rejects_output_dtype() {
        let error = FormulaSession::new(&fixture("formula_output_f32.onnx"), &rt())
            .expect_err("float output must fail");
        assert!(error.to_string().contains("INT64"), "error: {error}");
    }

    #[test]
    fn rejects_multi_input_and_output() {
        let input_error = FormulaSession::new(&fixture("formula_multi_input.onnx"), &rt())
            .expect_err("multi input must fail");
        assert!(
            input_error.to_string().contains("exactly one input"),
            "error: {input_error}"
        );
        let output_error = FormulaSession::new(&fixture("formula_multi_output.onnx"), &rt())
            .expect_err("multi output must fail");
        assert!(
            output_error.to_string().contains("exactly one output"),
            "error: {output_error}"
        );
    }

    #[test]
    fn wrong_input_shape_is_rejected_before_run() {
        let mut session = FormulaSession::new(&fixture("formula_ok.onnx"), &rt())
            .expect("formula fixture should load");
        let input = Array4::<f32>::zeros((1, 1, 512, 384));
        let error = session
            .run(input.view())
            .expect_err("bad input shape must fail");
        assert!(
            error.to_string().contains("[N,1,384,384]"),
            "error: {error}"
        );
    }

    /// 公式域不接受静默回退：即使 `fail_if_provider_unavailable=false`，
    /// 请求了当前构建未启用的 provider 也必须返回结构化错误。
    #[test]
    #[cfg(not(feature = "cuda-provider"))]
    fn unavailable_provider_is_rejected_even_without_strict_flag() {
        let runtime = RuntimeConfig {
            provider_preference: crate::config::ProviderPreference::Cuda { device_id: 0 },
            fail_if_provider_unavailable: false,
            ..RuntimeConfig::default()
        };
        let error = FormulaSession::new(&fixture("formula_ok.onnx"), &runtime)
            .expect_err("unavailable accelerator must not fall back to CPU");
        assert!(
            matches!(error, RapidOcrError::UnsupportedProvider(_)),
            "error: {error}"
        );
    }

    /// 显式请求 CPU 时必须正常构造，证明严格语义只拒绝“回退”，不拒绝 CPU。
    #[test]
    fn explicit_cpu_preference_is_accepted() {
        let runtime = RuntimeConfig {
            provider_preference: crate::config::ProviderPreference::Cpu,
            fail_if_provider_unavailable: true,
            ..RuntimeConfig::default()
        };
        let session = FormulaSession::new(&fixture("formula_ok.onnx"), &runtime)
            .expect("explicit CPU must be accepted");
        assert_eq!(
            session.provider_resolution().resolved,
            crate::runtime::provider::ResolvedExecutionProvider::Cpu
        );
    }
}

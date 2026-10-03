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

    /// 打开这个会话时下发给 ONNX Runtime 的 `(intra, inter)` 线程数。
    ///
    /// 值来自 [`RuntimeConfig::effective_session_threads`]——与引擎路径
    /// （[`crate::runtime::profile::RuntimeProfile::plan`]）用的是同一个函数。
    /// `None` 表示该线程数**未被配置**（ORT 用自己的默认值），不是 0 线程。
    pub fn session_threads(&self) -> (Option<usize>, Option<usize>) {
        self.inner.session_threads()
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
            session.provider_resolution().selected_ep,
            crate::runtime::provider::ResolvedExecutionProvider::Cpu
        );
    }

    /// **P1 根因回归（独立公式路径）**：`FormulaSession::new` 直接把 `RuntimeConfig` 交给
    /// `OrtSession`，因此它必须与引擎路径（`RuntimeProfile::plan`）得到**同一套**线程数。
    ///
    /// 旧行为：`OrtSession` 只按字段透传，默认配置（`auto_tune_threads = true`、没有显式
    /// 线程数）下公式路径**不配置** ORT 线程（拿到 ORT 默认值），而引擎路径把 intra 设为
    /// 预算（14）、inter 设为 1 —— 同一个公开字段在两个入口含义不同。
    ///
    /// 本测试用提交在仓库内的 fixture（`tests/fixtures/formula-onnx/formula_ok.onnx`），
    /// **不需要**外部 566 MB 公式模型，因此不会被 skip。
    #[test]
    fn standalone_session_uses_the_shared_thread_policy() {
        let runtime = RuntimeConfig::default();
        let expected = runtime.effective_session_threads();
        assert_eq!(
            expected,
            (
                Some(crate::runtime::profile::auto_tuned_thread_budget()),
                Some(1)
            ),
            "a default `RuntimeConfig` derives intra = budget and inter = 1"
        );

        let session = FormulaSession::new(&fixture("formula_ok.onnx"), &runtime)
            .expect("formula fixture should load");
        assert_eq!(
            session.session_threads(),
            expected,
            "the standalone formula path must ask ORT for the derived thread counts"
        );

        // 引擎路径必须给出同一对数字：两条路径共用 `effective_session_threads()`。
        let plan = crate::runtime::profile::RuntimeProfile::plan(&runtime, false);
        assert_eq!(
            (plan.threads.ort_intra, plan.threads.ort_inter),
            expected,
            "engine plan and standalone session must agree by construction"
        );
    }

    /// 显式值在独立公式路径上同样优先，且 `auto_tune_threads = false` 时未设置的字段
    /// 保持“不配置”。
    #[test]
    fn standalone_session_honours_explicit_threads() {
        let runtime = RuntimeConfig {
            intra_threads: Some(3),
            inter_threads: None,
            auto_tune_threads: false,
            ..RuntimeConfig::default()
        };
        assert_eq!(runtime.effective_session_threads(), (Some(3), None));
        let session = FormulaSession::new(&fixture("formula_ok.onnx"), &runtime)
            .expect("formula fixture should load");
        assert_eq!(session.session_threads(), (Some(3), None));
    }
}

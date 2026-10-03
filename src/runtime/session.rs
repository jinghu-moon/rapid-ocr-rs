use std::path::Path;

use ndarray::{Array2, ArrayView2, ArrayView3, ArrayViewD, Ix2, Ix3, Ix4};
use ort::{
    inputs,
    session::{Session, builder::GraphOptimizationLevel},
    value::TensorRef,
};

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    runtime::contracts::{ModelIoProbe, TensorSpec},
    runtime::provider::{ProviderResolution, resolve_execution_providers},
};

#[derive(Debug)]
pub struct OrtSession {
    session: Session,
    model_path: String,
    provider_resolution: ProviderResolution,
    /// 打开会话时下发给 ONNX Runtime 的 `(intra, inter)` 线程数；`None` 表示**没有配置**
    /// 该线程数（没有调用对应的 `with_*_threads`），ONNX Runtime 保留自己的默认值，
    /// 它不表示 0 线程。
    ///
    /// 这里的值是 [`RuntimeConfig::effective_session_threads`] 的输出，也就是“这份配置要求
    /// ORT 用什么”，不是 ORT 内部最终解析出的线程数（那不可观测）。
    session_threads: (Option<usize>, Option<usize>),
    pub output_names: Vec<String>,
}

fn ort_error<E: std::fmt::Display>(error: E) -> RapidOcrError {
    RapidOcrError::Decode(format!("ONNX Runtime error: {error}"))
}

impl OrtSession {
    /// Opens an ONNX model without domain contract validation.
    ///
    /// Domain sessions must call [`Self::probe_io`] and validate their typed
    /// contract through [`crate::runtime::contracts`].
    pub fn open_unchecked(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        let (session, provider_resolution, session_threads) =
            open_session(model_path, runtime_cfg)?;
        let output_names = session
            .outputs()
            .iter()
            .map(|value| value.name().to_string())
            .collect();
        Ok(Self {
            session,
            model_path: model_path.display().to_string(),
            provider_resolution,
            session_threads,
            output_names,
        })
    }

    /// 打开会话时下发给 ONNX Runtime 的 `(intra, inter)` 线程数。
    ///
    /// 直接构造 `RuntimeConfig` 的调用方（`FormulaSession` / `formula_bench` /
    /// `formula_eval`）可以用它证明自己拿到的是与引擎路径同一套线程解析结果。
    pub fn session_threads(&self) -> (Option<usize>, Option<usize>) {
        self.session_threads
    }

    pub fn metadata_custom(&self, key: &str) -> Result<Option<String>> {
        match self.session.metadata().map_err(ort_error)?.custom(key) {
            Some(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
            _ => Ok(None),
        }
    }

    pub fn probe_io(&self) -> Result<ModelIoProbe> {
        let inputs = self
            .session
            .inputs()
            .iter()
            .map(tensor_spec_from_outlet)
            .collect::<Result<Vec<_>>>()?;
        let outputs = self
            .session
            .outputs()
            .iter()
            .map(tensor_spec_from_outlet)
            .collect::<Result<Vec<_>>>()?;
        Ok(ModelIoProbe { inputs, outputs })
    }

    pub fn run_i64_2d(
        &mut self,
        input_name: &str,
        input: ndarray::ArrayView4<'_, f32>,
    ) -> Result<Array2<i64>> {
        let input_tensor = TensorRef::from_array_view(input).map_err(ort_error)?;
        let outputs = self
            .session
            .run(inputs![input_name => input_tensor])
            .map_err(ort_error)?;
        Self::output_i64_2d(outputs, &self.model_path, &self.output_names)
    }

    fn output_i64_2d(
        outputs: ort::session::SessionOutputs<'_>,
        model_path: &str,
        output_names: &[String],
    ) -> Result<Array2<i64>> {
        let output_name = output_names.first().ok_or_else(|| {
            RapidOcrError::Decode(format!(
                "ONNX session has no output names (model={model_path})"
            ))
        })?;
        let output = outputs.get(output_name.as_str()).ok_or_else(|| {
            RapidOcrError::Decode(format!(
                "ONNX session output `{output_name}` not found in run results (model={model_path})"
            ))
        })?;
        let arr = output.try_extract_array::<i64>().map_err(|error| {
            RapidOcrError::Decode(format!(
                "failed to extract output `{output_name}` as i64 tensor (model={model_path}): {error}"
            ))
        })?;
        arr.into_dimensionality::<Ix2>()
            .map(|view| view.to_owned())
            .map_err(|error| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {model_path}: expected rank2: {error}"
                ))
            })
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.provider_resolution
    }

    pub fn run_arrayd_view_with<T, F>(
        &mut self,
        input: ndarray::ArrayView4<'_, f32>,
        f: F,
    ) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayViewD<'a, f32>) -> Result<T>,
    {
        let input_tensor = TensorRef::from_array_view(input).map_err(ort_error)?;
        let outputs = self.session.run(inputs![input_tensor]).map_err(ort_error)?;
        let output_name = self.output_names.first().ok_or_else(|| {
            RapidOcrError::Decode(format!(
                "ONNX session has no output names (model={})",
                self.model_path
            ))
        })?;
        let output = outputs.get(output_name.as_str()).ok_or_else(|| {
            RapidOcrError::Decode(format!(
                "ONNX session output `{output_name}` not found in run results (model={})",
                self.model_path
            ))
        })?;
        let arr = output.try_extract_array::<f32>().map_err(|error| {
            RapidOcrError::Decode(format!(
                "failed to extract output `{output_name}` as f32 tensor (model={}): {error}",
                self.model_path
            ))
        })?;
        f(arr.view())
    }

    pub fn run_array2_view_with<T, F>(
        &mut self,
        input: ndarray::ArrayView4<'_, f32>,
        f: F,
    ) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView2<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix2>().map_err(|error| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {model_path}: expected rank2: {error}"
                ))
            })?;
            f(arr)
        })
    }

    pub fn run_array3_view_with<T, F>(
        &mut self,
        input: ndarray::ArrayView4<'_, f32>,
        f: F,
    ) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView3<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix3>().map_err(|error| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {model_path}: expected rank3: {error}"
                ))
            })?;
            f(arr)
        })
    }

    pub fn run_array4_view_with<T, F>(
        &mut self,
        input: ndarray::ArrayView4<'_, f32>,
        f: F,
    ) -> Result<T>
    where
        F: for<'a> FnOnce(ndarray::ArrayView4<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix4>().map_err(|error| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {model_path}: expected rank4: {error}"
                ))
            })?;
            f(arr)
        })
    }
}

/// [`open_session`] 的返回值：ORT 会话、provider 解析结果，以及实际下发给 ONNX Runtime 的
/// `(intra, inter)` 线程数（`None` = 该线程数未配置）。
type OpenedSession = (Session, ProviderResolution, (Option<usize>, Option<usize>));

/// 打开一个 ORT 会话。
///
/// 线程设置**不在这里推导**：调用
/// [`RuntimeConfig::effective_session_threads`]，那是本 crate 里唯一的线程策略实现
/// （显式值优先；否则按 `auto_tune_threads` 从共享预算推导；两者都没有时 `None`）。
/// 因此“直接构造 `RuntimeConfig` 的公式路径”和“先经过 `RuntimeProfile::plan` 的引擎路径”
/// 得到的是同一套语义。`None` 表示不调用 `with_intra_threads` / `with_inter_threads`，
/// 由 ONNX Runtime 使用自己的默认值。
///
/// 这里刻意**没有**第二份线程推导逻辑。历史实现有一个 `derive_runtime_threads`，它认为
/// `auto_tune_threads = false` 表示“不要自动配置”，而 `runtime::profile` 当时在同样的输入下
/// 无条件算出 `budget` 并当成显式值下发 —— 同一份 `RuntimeConfig` 通过 `RapidOcrEngine`
/// 与通过 `FormulaSession` / `formula_bench` / `formula_eval` 会得到不同线程行为；
/// 反向的偏差（引擎推导而公式路径只做透传、静默拿到 ORT 默认值）同样存在过。
/// 现在两个方向都只经过 [`RuntimeConfig::effective_session_threads`]。
fn open_session(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<OpenedSession> {
    // 只有 ONNX Runtime 一种后端：以前这里的单变体 `RuntimeBackend` 检查属于伪抽象，
    // 已随枚举一起删除。provider 差异完全由 `resolve_execution_providers` 表达。
    let mut builder = Session::builder().map_err(ort_error)?;
    builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort_error)?;
    let (intra, inter) = runtime_cfg.effective_session_threads();
    if let Some(intra) = intra {
        builder = builder.with_intra_threads(intra).map_err(ort_error)?;
    }
    if let Some(inter) = inter {
        builder = builder.with_inter_threads(inter).map_err(ort_error)?;
    }

    let provider_chain = resolve_execution_providers(
        &runtime_cfg.provider_preference,
        runtime_cfg.enable_cpu_mem_arena,
        runtime_cfg.fail_if_provider_unavailable,
    )?;
    builder = builder
        .with_execution_providers(provider_chain.providers)
        .map_err(ort_error)?;
    let session = builder.commit_from_file(model_path).map_err(ort_error)?;
    Ok((session, provider_chain.resolution, (intra, inter)))
}

fn tensor_spec_from_outlet(outlet: &ort::value::Outlet) -> Result<TensorSpec> {
    let dtype = outlet.dtype();
    if !dtype.is_tensor() {
        return Err(RapidOcrError::Config(format!(
            "model io `{}` must be a tensor, got `{dtype}`",
            outlet.name()
        )));
    }
    let shape = dtype.tensor_shape().ok_or_else(|| {
        RapidOcrError::Config(format!("model io `{}` has no tensor shape", outlet.name()))
    })?;
    let element_type = dtype.tensor_type().ok_or_else(|| {
        RapidOcrError::Config(format!(
            "model io `{}` has no tensor element type",
            outlet.name()
        ))
    })?;
    Ok(TensorSpec {
        name: outlet.name().to_string(),
        rank: shape.len(),
        dims: shape.to_vec(),
        element_type,
    })
}

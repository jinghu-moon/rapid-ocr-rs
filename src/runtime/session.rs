use std::path::Path;
use std::thread;

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
        let (session, provider_resolution) = open_session(model_path, runtime_cfg)?;
        let output_names = session
            .outputs()
            .iter()
            .map(|value| value.name().to_string())
            .collect();
        Ok(Self {
            session,
            model_path: model_path.display().to_string(),
            provider_resolution,
            output_names,
        })
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

fn derive_runtime_threads(runtime_cfg: &RuntimeConfig) -> (Option<usize>, Option<usize>) {
    let mut intra = runtime_cfg.intra_threads.filter(|value| *value > 0);
    let mut inter = runtime_cfg.inter_threads.filter(|value| *value > 0);

    if runtime_cfg.auto_tune_threads {
        let available = auto_tuned_thread_budget();
        if intra.is_none() {
            intra = Some(available.max(1));
        }
        if inter.is_none() {
            inter = Some(1);
        }
    }

    (intra, inter)
}

/// 自动调优时的线程预算：`min(逻辑核数, 物理核数)`，至少 1。
///
/// 唯一定义处：`runtime::profile` 用它推导统一的 `ThreadPlan`，`derive_runtime_threads`
/// 用它处理仍然走 `auto_tune_threads` 的调用方（公式基准等）。任何第二次实现都会让
/// “这个进程用了多少线程”重新失去单一解释处。
pub(crate) fn auto_tuned_thread_budget() -> usize {
    let physical_cores = num_cpus::get_physical().max(1);
    let available = thread::available_parallelism()
        .ok()
        .map(|value| value.get())
        .unwrap_or(1);
    available.clamp(1, physical_cores)
}

fn open_session(
    model_path: &Path,
    runtime_cfg: &RuntimeConfig,
) -> Result<(Session, ProviderResolution)> {
    // 只有 ONNX Runtime 一种后端：以前这里的单变体 `RuntimeBackend` 检查属于伪抽象，
    // 已随枚举一起删除。provider 差异完全由 `resolve_execution_providers` 表达。
    let mut builder = Session::builder().map_err(ort_error)?;
    builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort_error)?;
    let (intra_threads, inter_threads) = derive_runtime_threads(runtime_cfg);
    if let Some(intra) = intra_threads {
        builder = builder.with_intra_threads(intra).map_err(ort_error)?;
    }
    if let Some(inter) = inter_threads {
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
    Ok((session, provider_chain.resolution))
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

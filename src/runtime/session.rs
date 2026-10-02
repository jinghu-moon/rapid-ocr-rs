use std::path::Path;
use std::thread;

use ndarray::{Array2, ArrayView2, ArrayView3, ArrayView4, ArrayViewD, Ix2, Ix3, Ix4};
use ort::{
    inputs,
    session::{Session, builder::GraphOptimizationLevel},
    value::{TensorElementType, TensorRef, ValueType},
};

use crate::{
    config::{RuntimeBackend, RuntimeConfig},
    error::{RapidOcrError, Result},
    runtime::provider::{ProviderResolution, resolve_execution_providers},
};

#[derive(Debug)]
pub struct OrtSession {
    session: Session,
    model_path: String,
    provider_resolution: ProviderResolution,
    pub output_names: Vec<String>,
    pub character_list: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionContract {
    Rec,
    Cls,
    Det,
}

/// 通用 ONNX IO 契约描述（供公式探针与未来领域契约使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorSpec {
    pub name: String,
    pub rank: usize,
    pub dims: Vec<i64>,
    pub element_type: TensorElementType,
}

/// 对已加载 session 进行通用 IO 探测的结果。
#[derive(Debug, Clone)]
pub struct ModelIoProbe {
    pub inputs: Vec<TensorSpec>,
    pub outputs: Vec<TensorSpec>,
}

fn ort_error<E: std::fmt::Display>(error: E) -> RapidOcrError {
    RapidOcrError::Decode(format!("ONNX Runtime error: {error}"))
}

impl OrtSession {
    pub fn new(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        Self::new_with_contract(model_path, runtime_cfg, SessionContract::Rec)
    }

    pub fn new_with_contract(
        model_path: &Path,
        runtime_cfg: &RuntimeConfig,
        contract: SessionContract,
    ) -> Result<Self> {
        let (session, provider_resolution) = open_session(model_path, runtime_cfg)?;
        validate_model_io_contract(model_path, &session, contract)?;
        Ok(Self::finish(
            session,
            model_path,
            provider_resolution,
            runtime_cfg,
        ))
    }

    /// 打开 ONNX 模型而不做任何领域契约校验。
    ///
    /// 供公式探针等对模型签名/元数据做独立检查的调用方使用；普通 OCR 必须使用
    /// [`Self::new_with_contract`] 以保持契约防线。
    pub fn open_unchecked(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        let (session, provider_resolution) = open_session(model_path, runtime_cfg)?;
        Ok(Self::finish(
            session,
            model_path,
            provider_resolution,
            runtime_cfg,
        ))
    }

    /// 读取自定义 metadata 键的原始字符串（如 `character`）。
    pub fn metadata_custom(&self, key: &str) -> Result<Option<String>> {
        match self.session.metadata().map_err(ort_error)?.custom(key) {
            Some(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
            _ => Ok(None),
        }
    }

    /// 探测模型输入/输出签名（名称、rank、维度、元素类型），不做任何假设。
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

    /// 以命名输入运行，第一个输出按 `INT64` rank-2 提取为 owned `Array2<i64>`。
    ///
    /// 用于公式 token 序列输出；非 `INT64`/非 rank-2 时返回明确错误。
    pub fn run_i64_2d(
        &mut self,
        input_name: &str,
        input: ArrayView4<'_, f32>,
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
        let arr = output.try_extract_array::<i64>().map_err(|e| {
            RapidOcrError::Decode(format!(
                "failed to extract output `{output_name}` as i64 tensor (model={model_path}): {e}"
            ))
        })?;
        let rank2 = arr.into_dimensionality::<Ix2>().map_err(|e| {
            RapidOcrError::Decode(format!(
                "unexpected output rank for model {model_path}: expected rank2: {e}"
            ))
        })?;
        Ok(rank2.to_owned())
    }

    fn finish(
        session: Session,
        model_path: &Path,
        provider_resolution: ProviderResolution,
        runtime_cfg: &RuntimeConfig,
    ) -> Self {
        let output_names = session
            .outputs()
            .iter()
            .map(|v| v.name().to_string())
            .collect();
        let character_list = match session.metadata().map_err(ort_error) {
            Ok(metadata) => match metadata.custom("character") {
                Some(raw) if !raw.trim().is_empty() => {
                    Some(raw.lines().map(|line| line.to_string()).collect())
                }
                _ => None,
            },
            Err(_) => None,
        };
        let _ = runtime_cfg;
        Self {
            session,
            model_path: model_path.display().to_string(),
            provider_resolution,
            output_names,
            character_list,
        }
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.provider_resolution
    }

    pub fn run_arrayd_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
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

        let arr = output.try_extract_array::<f32>().map_err(|e| {
            RapidOcrError::Decode(format!(
                "failed to extract output `{output_name}` as f32 tensor (model={}): {e}",
                self.model_path
            ))
        })?;
        f(arr.view())
    }

    pub fn run_array2_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView2<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix2>().map_err(|e| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {}: expected rank2: {e}",
                    model_path
                ))
            })?;
            f(arr)
        })
    }

    pub fn run_array3_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView3<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix3>().map_err(|e| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {}: expected rank3: {e}",
                    model_path
                ))
            })?;
            f(arr)
        })
    }

    pub fn run_array4_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ndarray::ArrayView4<'a, f32>) -> Result<T>,
    {
        let model_path = self.model_path.clone();
        self.run_arrayd_view_with(input, |arr| {
            let arr = arr.into_dimensionality::<Ix4>().map_err(|e| {
                RapidOcrError::Decode(format!(
                    "unexpected output rank for model {}: expected rank4: {e}",
                    model_path
                ))
            })?;
            f(arr)
        })
    }
}

fn derive_runtime_threads(runtime_cfg: &RuntimeConfig) -> (Option<usize>, Option<usize>) {
    let mut intra = runtime_cfg.intra_threads.filter(|v| *v > 0);
    let mut inter = runtime_cfg.inter_threads.filter(|v| *v > 0);

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

fn auto_tuned_thread_budget() -> usize {
    let physical_cores = num_cpus::get_physical().max(1);
    let available = thread::available_parallelism()
        .ok()
        .map(|v| v.get())
        .unwrap_or(1);
    available.clamp(1, physical_cores)
}

/// 共享 session 打开逻辑：backends、provider、线程与优化级别。
fn open_session(
    model_path: &Path,
    runtime_cfg: &RuntimeConfig,
) -> Result<(Session, ProviderResolution)> {
    if runtime_cfg.backend != RuntimeBackend::OnnxCpu {
        return Err(RapidOcrError::UnsupportedBackend(format!(
            "only `onnx_cpu` is supported in this release, got {:?}",
            runtime_cfg.backend
        )));
    }

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

fn validate_model_io_contract(
    model_path: &Path,
    session: &Session,
    contract: SessionContract,
) -> Result<()> {
    if session.inputs().len() != 1 {
        return Err(RapidOcrError::Config(format!(
            "recognition model must expose exactly one input, got {} (model={})",
            session.inputs().len(),
            model_path.display()
        )));
    }
    if session.outputs().is_empty() {
        return Err(RapidOcrError::Config(format!(
            "recognition model must expose at least one output (model={})",
            model_path.display()
        )));
    }

    let input = &session.inputs()[0];
    validate_tensor_spec(
        model_path,
        "input",
        input.name(),
        input.dtype(),
        Some(4),
        TensorElementType::Float32,
    )?;

    let output = &session.outputs()[0];
    let output_rank = match contract {
        SessionContract::Rec => Some(3),
        SessionContract::Cls => Some(2),
        SessionContract::Det => Some(4),
    };
    validate_tensor_spec(
        model_path,
        "output",
        output.name(),
        output.dtype(),
        output_rank,
        TensorElementType::Float32,
    )?;

    Ok(())
}

fn validate_tensor_spec(
    model_path: &Path,
    io_kind: &str,
    io_name: &str,
    value_type: &ValueType,
    expected_rank: Option<usize>,
    expected_tensor_type: TensorElementType,
) -> Result<()> {
    if !value_type.is_tensor() {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{io_name}` must be a tensor, got `{value_type}` (model={})",
            model_path.display()
        )));
    }

    let actual_rank = value_type
        .tensor_shape()
        .map(|shape| shape.len())
        .unwrap_or_default();
    if let Some(expected_rank) = expected_rank
        && actual_rank != expected_rank
    {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{io_name}` rank mismatch: expected {expected_rank}, got {actual_rank} (type={value_type}, model={})",
            model_path.display()
        )));
    }

    let actual_type = value_type.tensor_type().ok_or_else(|| {
        RapidOcrError::Config(format!(
            "model {io_kind} `{io_name}` has no tensor element type (type={value_type}, model={})",
            model_path.display()
        ))
    })?;

    if actual_type != expected_tensor_type {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{io_name}` dtype mismatch: expected {:?}, got {:?} (model={})",
            expected_tensor_type,
            actual_type,
            model_path.display()
        )));
    }

    Ok(())
}

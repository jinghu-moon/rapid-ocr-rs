//! 共享 ONNX IO 契约验证工具。
//!
//! 该模块只描述 tensor 名称、rank、维度与元素类型，不绑定普通 OCR 或公式领域。
//! 领域 session 使用这些工具构造自己的 typed contract。

use std::path::Path;

use ort::value::TensorElementType;

use crate::error::{RapidOcrError, Result};

/// 通用 ONNX tensor 签名描述。
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

pub fn require_single_input<'a>(
    probe: &'a ModelIoProbe,
    model_path: &Path,
    model_kind: &str,
) -> Result<&'a TensorSpec> {
    if probe.inputs.len() != 1 {
        return Err(RapidOcrError::Config(format!(
            "{model_kind} model must expose exactly one input, got {} (model={})",
            probe.inputs.len(),
            model_path.display()
        )));
    }
    Ok(&probe.inputs[0])
}

pub fn require_single_output<'a>(
    probe: &'a ModelIoProbe,
    model_path: &Path,
    model_kind: &str,
) -> Result<&'a TensorSpec> {
    if probe.outputs.len() != 1 {
        return Err(RapidOcrError::Config(format!(
            "{model_kind} model must expose exactly one output, got {} (model={})",
            probe.outputs.len(),
            model_path.display()
        )));
    }
    Ok(&probe.outputs[0])
}

pub fn require_tensor(
    spec: &TensorSpec,
    io_kind: &str,
    expected_rank: usize,
    expected_type: TensorElementType,
    model_path: &Path,
) -> Result<()> {
    if spec.rank != expected_rank {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{}` rank mismatch: expected {expected_rank}, got {} (model={})",
            spec.name,
            spec.rank,
            model_path.display()
        )));
    }
    if spec.element_type != expected_type {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{}` dtype mismatch: expected {}, got {} (model={})",
            spec.name,
            element_type_label(expected_type),
            element_type_label(spec.element_type),
            model_path.display()
        )));
    }
    Ok(())
}

fn element_type_label(element_type: TensorElementType) -> String {
    match element_type {
        TensorElementType::Float32 => "FLOAT32".to_string(),
        TensorElementType::Float16 => "FLOAT16".to_string(),
        TensorElementType::Int64 => "INT64".to_string(),
        TensorElementType::Int32 => "INT32".to_string(),
        TensorElementType::Uint8 => "UINT8".to_string(),
        TensorElementType::Int8 => "INT8".to_string(),
        other => format!("{other:?}"),
    }
}

pub fn require_fixed_dim(
    spec: &TensorSpec,
    io_kind: &str,
    dim_index: usize,
    expected: i64,
    model_path: &Path,
) -> Result<()> {
    let Some(actual) = spec.dims.get(dim_index) else {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{}` is missing dim index {dim_index} (dims={:?}, model={})",
            spec.name,
            spec.dims,
            model_path.display()
        )));
    };
    if *actual != expected {
        return Err(RapidOcrError::Config(format!(
            "model {io_kind} `{}` dim mismatch at index {dim_index}: expected {expected}, got {actual} (dims={:?}, model={})",
            spec.name,
            spec.dims,
            model_path.display()
        )));
    }
    Ok(())
}

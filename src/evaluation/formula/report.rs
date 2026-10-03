//! 公式评测结果记录、失败分类与聚合。
//!
//! 任务文档要求“将文本解码错误与模型识别错误分开统计”，因此每个样本都必须被
//! 归入一个明确的 [`FailureKind`]，而不是只看 `error` 字段是否为空：
//!
//! - 链路失败（图片解码 / ONNX 推理 / tokenizer 解码）说明 Rust 与参考实现的
//!   执行链路存在差异，必须单独统计；
//! - 链路正常但 LaTeX 与真值不符属于**模型识别错误**，不能算作链路问题；
//! - 无 EOS 的截断结果是结构化输出，单独计数而不吞掉。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    error::RapidOcrError,
    evaluation::formula::metrics::{FormulaTextMetrics, evaluate_text},
};

/// 单样本失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// 链路正常且 LaTeX 与真值一致。
    None,
    /// 图片读取或解码失败。
    ImageDecode,
    /// ONNX / ORT 推理失败。
    Inference,
    /// tokenizer 或 LaTeX 解码失败（“文本解码错误”）。
    TokenizerDecode,
    /// 输入受限或参数非法。
    InputRejected,
    /// 链路正常但输出无 EOS：结果被截断。
    TruncatedNoEos,
    /// 链路正常但与真值不一致（“模型识别错误”）。
    ModelMismatch,
    /// 真值为空，排除出精确匹配统计。
    EmptyGroundTruth,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ImageDecode => "image_decode",
            Self::Inference => "inference",
            Self::TokenizerDecode => "tokenizer_decode",
            Self::InputRejected => "input_rejected",
            Self::TruncatedNoEos => "truncated_no_eos",
            Self::ModelMismatch => "model_mismatch",
            Self::EmptyGroundTruth => "empty_ground_truth",
        }
    }

    /// 是否属于 Rust/ONNX/tokenizer 链路问题（相对参考实现的差异候选）。
    pub fn is_pipeline_error(self) -> bool {
        matches!(
            self,
            Self::ImageDecode | Self::Inference | Self::TokenizerDecode | Self::InputRejected
        )
    }

    /// 是否属于模型识别质量问题（链路正常）。
    pub fn is_model_error(self) -> bool {
        matches!(self, Self::ModelMismatch | Self::TruncatedNoEos)
    }
}

/// 把 `RapidOcrError` 归类到失败种类；`None` 表示链路本身没有失败。
pub fn classify_error(error: &RapidOcrError) -> FailureKind {
    match error {
        RapidOcrError::InvalidImage(_) => FailureKind::ImageDecode,
        RapidOcrError::FileNotFound(_) => FailureKind::ImageDecode,
        RapidOcrError::Tokenizer(_) => FailureKind::TokenizerDecode,
        RapidOcrError::Decode(_) => FailureKind::Inference,
        RapidOcrError::InvalidInput(_) => FailureKind::InputRejected,
        RapidOcrError::Config(_) => FailureKind::InputRejected,
        RapidOcrError::UnsupportedProvider(_) => FailureKind::InputRejected,
        RapidOcrError::UnsupportedBackend(_) => FailureKind::InputRejected,
        RapidOcrError::HashMismatch { .. } => FailureKind::InputRejected,
        RapidOcrError::ModelResolve(_) => FailureKind::InputRejected,
        // 公式评测只从本地路径加载模型（不下载），因此 `Download` 在这一层来自
        // "远端图片取不回来"（`input/image_loader.rs`），归类为 ImageDecode 保持不变。
        RapidOcrError::Download(_) => FailureKind::ImageDecode,
        RapidOcrError::Io(_) => FailureKind::ImageDecode,
        RapidOcrError::Reqwest(_) => FailureKind::ImageDecode,
        RapidOcrError::Yaml(_) => FailureKind::InputRejected,
    }
}

/// 单个评测样本的记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleRecord {
    /// 绝对路径，便于人工定位。
    pub image: String,
    /// 相对数据集根目录的路径（`/` 分隔），与 manifest 一一对应，
    /// 也是与 Python 参考对齐的稳定键，避免平台路径差异。
    pub relative_path: String,
    pub expected: String,
    pub actual: Option<String>,
    pub token_ids: Vec<i64>,
    pub eos_index: Option<usize>,
    pub truncated: Option<bool>,
    /// 产生该结果的识别调用墙钟耗时（毫秒）；batch 调用中为该 batch 的总耗时。
    pub elapsed_ms: f32,
    /// 该调用包含的图片数量；用于把 `elapsed_ms` 折算成单图耗时。
    pub batch_size: usize,
    pub failure: FailureKind,
    pub error: Option<String>,
    pub exact_match: bool,
    pub normalized_match: bool,
    pub edit_distance: usize,
    pub cer: f64,
    /// 与 Python 参考的 token 序列比较；仅在对比运行时填充。
    pub python_token_match: Option<bool>,
    pub python_latex_match: Option<bool>,
}

impl SampleRecord {
    /// 链路失败记录；`relative_path` 由调用方在 manifest 对齐后补齐。
    pub fn pipeline_error(image: String, expected: String, error: &RapidOcrError) -> Self {
        Self {
            image,
            relative_path: String::new(),
            expected,
            actual: None,
            token_ids: Vec::new(),
            eos_index: None,
            truncated: None,
            elapsed_ms: 0.0,
            batch_size: 0,
            failure: classify_error(error),
            error: Some(error.to_string()),
            exact_match: false,
            normalized_match: false,
            edit_distance: usize::MAX,
            cer: 1.0,
            python_token_match: None,
            python_latex_match: None,
        }
    }
}

/// 聚合指标。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvaluationSummary {
    pub total: usize,
    /// 参与精确/归一化匹配统计的样本数（排除空真值）。
    pub scored: usize,
    pub pipeline_failures: usize,
    pub model_mismatches: usize,
    pub exact_matches: usize,
    pub normalized_matches: usize,
    pub truncated: usize,
    pub exact_match_rate: f64,
    pub normalized_match_rate: f64,
    pub mean_cer: f64,
    pub failure_counts: BTreeMap<String, usize>,
}

/// 根据已评分样本的指标与失败分类聚合汇总。
pub fn summarize<'a, I>(records: I) -> EvaluationSummary
where
    I: IntoIterator<Item = &'a SampleRecord>,
{
    let mut summary = EvaluationSummary::default();
    let mut cer_sum = 0.0f64;

    for record in records {
        summary.total += 1;
        *summary
            .failure_counts
            .entry(record.failure.as_str().to_string())
            .or_insert(0) += 1;
        if record.failure.is_pipeline_error() {
            summary.pipeline_failures += 1;
        }
        if record.failure == FailureKind::ModelMismatch {
            summary.model_mismatches += 1;
        }
        if record.truncated == Some(true) {
            summary.truncated += 1;
        }
        if record.failure == FailureKind::EmptyGroundTruth || record.failure.is_pipeline_error() {
            continue;
        }
        summary.scored += 1;
        if record.exact_match {
            summary.exact_matches += 1;
        }
        if record.normalized_match {
            summary.normalized_matches += 1;
        }
        cer_sum += record.cer;
    }

    if summary.scored > 0 {
        summary.exact_match_rate = summary.exact_matches as f64 / summary.scored as f64;
        summary.normalized_match_rate = summary.normalized_matches as f64 / summary.scored as f64;
        summary.mean_cer = cer_sum / summary.scored as f64;
    }
    summary
}

/// 计算链路正常样本的文本指标并给出失败分类。
///
/// 分类优先级：空真值 > 精确匹配（无失败）> 无 EOS 截断 > 模型不匹配。
/// 无 EOS 且恰好精确匹配的样本不算失败，但它仍然会计入 `summary.truncated`，
/// 因为“模型没有给出结束标记”这一事实必须保持可见。
pub fn score_latex(
    expected: &str,
    actual: &str,
    truncated: bool,
) -> (FormulaTextMetrics, FailureKind) {
    let metrics = evaluate_text(expected, actual);
    let kind = if expected.trim().is_empty() {
        FailureKind::EmptyGroundTruth
    } else if metrics.exact_match {
        FailureKind::None
    } else if truncated {
        FailureKind::TruncatedNoEos
    } else {
        FailureKind::ModelMismatch
    };
    (metrics, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(expected: &str, actual: &str, truncated: bool) -> SampleRecord {
        let (metrics, failure) = score_latex(expected, actual, truncated);
        SampleRecord {
            image: "img.png".to_string(),
            relative_path: "img.png".to_string(),
            expected: expected.to_string(),
            actual: Some(actual.to_string()),
            token_ids: vec![0, 2],
            eos_index: Some(1),
            truncated: Some(truncated),
            elapsed_ms: 1.0,
            batch_size: 1,
            failure,
            error: None,
            exact_match: metrics.exact_match,
            normalized_match: metrics.normalized_match,
            edit_distance: metrics.edit_distance,
            cer: metrics.cer,
            python_token_match: None,
            python_latex_match: None,
        }
    }

    #[test]
    fn pipeline_and_model_errors_are_counted_separately() {
        let pipeline = SampleRecord::pipeline_error(
            "a.png".to_string(),
            "x".to_string(),
            &RapidOcrError::Tokenizer("bad token".to_string()),
        );
        assert_eq!(pipeline.failure, FailureKind::TokenizerDecode);
        assert!(pipeline.failure.is_pipeline_error());

        let records = [
            record("x+y", "x+y", false),
            record("x+y", "x-y", false),
            record("x+y", "x+y", true),
            pipeline,
            record("", "", false),
        ];
        let summary = summarize(records.iter());
        assert_eq!(summary.total, 5);
        assert_eq!(
            summary.scored, 3,
            "pipeline error and empty truth are not scored"
        );
        assert_eq!(summary.pipeline_failures, 1);
        assert_eq!(
            summary.model_mismatches, 1,
            "only the wrong LaTeX is a model error"
        );
        assert_eq!(summary.exact_matches, 2);
        assert_eq!(
            summary.truncated, 1,
            "truncation stays visible even when the LaTeX happens to match"
        );
        assert_eq!(summary.failure_counts["empty_ground_truth"], 1);
        assert_eq!(
            summary
                .failure_counts
                .get("truncated_no_eos")
                .copied()
                .unwrap_or(0),
            0
        );
        assert!((summary.exact_match_rate - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn truncated_output_is_a_model_error_not_a_pipeline_error() {
        let (_, failure) = score_latex("x+y", "x+", true);
        assert_eq!(failure, FailureKind::TruncatedNoEos);
        assert!(failure.is_model_error());
        assert!(!failure.is_pipeline_error());
    }

    #[test]
    fn error_classification_covers_decode_and_inference() {
        assert_eq!(
            classify_error(&RapidOcrError::Decode("ort".to_string())),
            FailureKind::Inference
        );
        assert_eq!(
            classify_error(&RapidOcrError::InvalidImage("png".to_string())),
            FailureKind::ImageDecode
        );
        assert_eq!(
            classify_error(&RapidOcrError::Tokenizer("vocab".to_string())),
            FailureKind::TokenizerDecode
        );
    }

    #[test]
    fn normalized_match_is_tracked_separately_from_exact() {
        let (metrics, failure) = score_latex("x + y", "x+y", false);
        assert!(!metrics.exact_match);
        assert!(metrics.normalized_match);
        assert_eq!(failure, FailureKind::ModelMismatch);
    }
}

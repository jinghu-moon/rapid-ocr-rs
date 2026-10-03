//! 公开 API 的唯一重导出出口。
//!
//! 这里只做 `pub use`，不放实现。集中在一个模块里的原因有两个：
//!
//! 1. 平台谓词只需要在 `lib.rs` 写一次，而不是在十几个 `pub use` 上重复；
//! 2. 审查公开面时只需要读这一个文件（阶段 7 的 API 清理门槛）。
//!
//! 模块可见性仍然由各自的 `mod.rs` 决定：本文件只把既有的 `pub` 项提升到 crate 根部。

pub use crate::api::{
    ClassifierPlan, ClassifierPolicy, CoordinateSpace, DetectionOutcome, DetectionPolicy,
    EngineInfo, EnhancementPolicy, FormulaOutcome, FormulaPolicy, ImageInfo, ImageInput, ImageSize,
    InputTimings, ModelArtifact, ModelManifest, ModelSource, OcrEngine, OcrOutput, OcrRegion,
    OcrRequest, OcrTimings, OcrWord, OutputPolicy, OwnedPixelBuffer, PixelFormat, Polygon,
    PreprocessPolicy, ProviderInfo, ProviderPreference as GenericProviderPreference,
    ProviderResolutionInfo, RecognitionOutcome, RecognitionPolicy, RectU32, RegionKind,
    RegionSource, ResolvedProvider, StagePlan, StageReport, StageReports, StageState, StageTiming,
    TextOrder, TextOrientation, TilePolicy, WordKind, WordOutputMode,
};
pub use crate::config::{
    ColorOrder, LangCls, LangDet, LangRec, ModelType, OcrVersion, ProviderPreference, RecImage,
    RuntimeBackend, RuntimeConfig, VisionBackend,
};
pub use crate::error::{RapidOcrError, Result};
pub use crate::formula::contract::{
    FORMULA_INPUT_CHANNELS, FORMULA_INPUT_RANK, FORMULA_INPUT_SPATIAL, FORMULA_OUTPUT_RANK,
    FormulaContract, validate_formula_contract,
};
pub use crate::formula::detect::{
    DEFAULT_FORMULA_DETECT_CONFIDENCE, DEFAULT_FORMULA_DETECT_IOU,
    DEFAULT_FORMULA_DETECT_MAX_DETECTIONS, FORMULA_DETECT_INPUT_SIZE, FormulaBox,
    FormulaDetectOptions, FormulaDetector,
};
pub use crate::formula::ftfy::{NOT_APPLICABLE_STEPS, UNIMPLEMENTED_STEPS, fix_text};
pub use crate::formula::model_info::FormulaModelInfo;
pub use crate::formula::output::{
    order_formula_results, to_formula_html, to_formula_json, to_formula_markdown,
};
pub use crate::formula::preprocess::{
    FORMULA_INPUT_SIZE, FORMULA_MEAN, FORMULA_STD, FormulaPreprocessor, FormulaTensor,
};
pub use crate::formula::recognizer::{
    DEFAULT_MAX_FORMULA_BATCH_SIZE, DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH, FORMULA_MODEL_LOOP_BOUND,
    FormulaRecognition, FormulaRecognizer,
};
pub use crate::formula::session::FormulaSession;
pub use crate::formula::tokenizer::{FormulaDecode, FormulaTokenizer};
pub use crate::formula::tokenizer_metadata::{
    BOS_ID, BOS_TOKEN, EOS_ID, EOS_TOKEN, FormulaTokenizerMetadata, PAD_ID, PAD_TOKEN, UNK_ID,
    UNK_TOKEN,
};
pub use crate::model_store::{
    default_model_store_dir, ensure_downloaded, sha256_file, verify_existing_file,
};
pub use crate::ocr::config::{ModelConfig, RecognizeOptions, RecognizerConfig};
pub use crate::ocr::pipeline::{
    config::{EngineConfig, GlobalConfig},
    rapid_ocr::{PipelineProviderResolutions, RapidOcr, RapidOcrEngine},
};
pub use crate::ocr::types::{LineResult, RecognizeOutput, WordBox, WordInfo, WordType};
pub use crate::output::html::{relative_image_name, render_output_report, render_report};
pub use crate::output::json::{OcrJsonItem, to_output_items, to_output_json};
pub use crate::output::markdown::to_output_markdown;
pub use crate::output::visualize::draw_output;
pub use crate::runtime::contracts::{ModelIoProbe, TensorSpec};
pub use crate::runtime::memory::{
    PEAK_MEMORY_SOURCE, peak_memory_failure_reason, peak_memory_source, peak_working_set_bytes,
};
pub use crate::runtime::provider::{ProviderResolution, ResolvedExecutionProvider};
pub use crate::runtime::session::OrtSession;

/// 四边形（左上、右上、右下、左下）。
pub type Quad = [[f32; 2]; 4];

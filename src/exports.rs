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
    InputTimings, OcrEngine, OcrOutput, OcrRegion, OcrRequest, OcrTimings, OcrWord, OutputPolicy,
    OwnedPixelBuffer, PixelFormat, Polygon, PreprocessPolicy, ProviderInfo,
    ProviderPreference as GenericProviderPreference, ProviderResolutionInfo, RecognitionOutcome,
    RecognitionPolicy, RectU32, RegionKind, RegionSource, ResolvedProvider, StagePlan, StageReport,
    StageReports, StageState, StageTiming, TextOrder, TextOrientation, TilePolicy, WordKind,
    WordOutputMode,
};
pub use crate::config::{
    ColorOrder, LangCls, LangDet, LangRec, ModelType, OcrVersion, ProviderPreference, RecImage,
    RuntimeConfig,
};
pub use crate::error::{RapidOcrError, Result};
// 输入层：`ImageInput::Encoded` 在管线内部走的就是这两个类型。导出它们是为了让
// `rapidocr serve` 的 `/annotated.png` 能**复用同一个**解码实现（编码字节上限、header
// 像素探测、EXIF 方向、解码错误语义只有一份），而不是在 serve 里另写一套图片解码
// （docs/05 §4.5：原图只保留编码字节，注释图按需重新解码）。
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
pub use crate::input::image_loader::{LoadImage, OcrInput};
pub use crate::model_registry::DefaultModelSelection;
pub use crate::model_set::{
    ModelFileSpec, ModelFileState, ModelRole, ModelSet, ModelSetStatus, model_set_status,
    model_set_status_probed, validate_model_file_name, validate_model_files,
    validate_model_files_probed,
};
pub use crate::model_source::{
    MANIFEST_FILE_NAME, ManifestFile, ModelManifest, ModelRequest, ModelSource, ModelSourceKind,
    SUPPORTED_MANIFEST_SCHEMA_VERSION,
};
// 文件身份键控的 SHA-256 校验缓存（`src/model_verify.rs`）：`/api/models` 的逐文件状态、
// 公式队列的准入判定与库的两条公式加载路径共用**同一份**校验证据。
// `verification_stats` 是"第一次验证真的哈希了、之后只花一次 stat"的如实成本账。
pub use crate::model_verify::{
    FileIdentity, VerificationOutcome, VerificationStats, clear_verification_cache,
    sha256_file_cached, verification_stats, verify_file, verify_sha256,
};
// 加固下载器（`docs/05` §6）：库侧的**唯一**下载入口与唯一的错误分类。
// `ensure_downloaded`（可传 `None` 哈希的旧入口）已按 §6.4 删除，不保留兼容层。
pub use crate::model_store::{
    ALLOWED_DOWNLOAD_HOSTS, DEFAULT_ALLOWED_HOSTS, DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_MAX_DOWNLOAD_BYTES, DEFAULT_MAX_DOWNLOAD_MB, DEFAULT_READ_TIMEOUT, DownloadBudget,
    DownloadError, DownloadObserver, DownloadRequest, MAX_REDIRECT_HOPS, NoObserver,
    available_disk_bytes, default_model_store_dir, download_model_set, download_model_set_observed,
    download_verified, require_model_hash, sha256_file, verify_existing_file,
};
pub use crate::ocr::config::{ModelConfig, RecognizeOptions, RecognizerConfig};
pub use crate::ocr::pipeline::{
    config::{EngineConfig, GlobalConfig},
    rapid_ocr::{PipelineProviderResolutions, RapidOcr, RapidOcrEngine},
};
pub use crate::ocr::types::{LineResult, RecognizeOutput, WordBox, WordInfo, WordType};
pub use crate::output::html::{
    ReportMode, relative_image_name, render_output_report, render_report,
};
pub use crate::output::json::{OcrJsonItem, to_output_items, to_output_json};
pub use crate::output::markdown::to_output_markdown;
pub use crate::output::visualize::draw_output;
pub use crate::runtime::contracts::{ModelIoProbe, TensorSpec};
pub use crate::runtime::memory::{
    PEAK_MEMORY_SOURCE, peak_memory_failure_reason, peak_memory_source, peak_working_set_bytes,
};
pub use crate::runtime::ort_runtime::{
    LoadedModule, OrtRuntimeFingerprint, PROVIDER_DLL_NAMES, ProviderDll, is_sha256_hex,
    ort_runtime_fingerprint,
};
pub use crate::runtime::profile::{RuntimeProfile, ThreadPlan, ThreadSource};
// `resolve_execution_providers` 与 `format_provider_preference` 是 serve 侧启动期
// 校验运行配置（§7.5/§7.6）时**必须复用**的两个既有实现：前者给出"provider 名称/
// feature 是否成立 + 运行库是否可用"的唯一判定，后者给出 provider 的唯一展示文本。
// 导出的目的是让 `rapidocr serve` 不另写一套措辞（docs/05 §7.5 第 3 条）。
pub use crate::runtime::provider::{
    ProviderResolution, ResolvedExecutionProvider, format_provider_preference, ort_runtime_version,
    resolve_execution_providers,
};
pub use crate::runtime::session::OrtSession;
pub use crate::runtime::timing::{LedgerConservation, LedgerShares, TimingLedger};

/// 四边形（左上、右上、右下、左下）。
pub type Quad = [[f32; 2]; 4];

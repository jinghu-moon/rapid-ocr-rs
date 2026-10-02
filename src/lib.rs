mod api;
mod config;
mod error;
pub mod evaluation;
mod formula;
mod input;
mod model_registry;
mod model_store;
mod ocr;
mod output;
mod runtime;
mod vision;

pub use api::{
    ClassifierPlan, ClassifierPolicy, CoordinateSpace, DetectionOutcome, DetectionPolicy,
    EngineInfo, EnhancementPolicy, ImageInfo, ImageInput, ImageSize, InputTimings, ModelArtifact,
    ModelManifest, ModelSource, OcrEngine, OcrOutput, OcrRegion, OcrRequest, OcrTimings, OcrWord,
    OutputPolicy, OwnedPixelBuffer, PixelFormat, Polygon, PreprocessPolicy, ProviderInfo,
    ProviderPreference as GenericProviderPreference, ProviderResolutionInfo, RecognitionOutcome,
    RecognitionPolicy, RectU32, RegionSource, ResolvedProvider, StagePlan, StageReport,
    StageReports, StageState, StageTiming, TextOrder, TextOrientation, TilePolicy, WordKind,
    WordOutputMode,
};
pub use config::{
    ColorOrder, LangCls, LangDet, LangRec, ModelType, OcrVersion, ProviderPreference, RecImage,
    RuntimeBackend, RuntimeConfig, VisionBackend,
};
pub use error::{RapidOcrError, Result};
pub use formula::model_info::{
    FORMULA_INPUT_CHANNELS, FORMULA_INPUT_RANK, FORMULA_INPUT_SPATIAL, FORMULA_OUTPUT_RANK,
    FormulaModelInfo,
};
pub use formula::preprocess::{
    FORMULA_INPUT_SIZE, FORMULA_MEAN, FORMULA_STD, FormulaPreprocessor, FormulaTensor,
};
pub use formula::session::FormulaSession;
pub use formula::tokenizer::{FormulaDecode, FormulaTokenizer};
pub use formula::tokenizer_metadata::{
    BOS_ID, BOS_TOKEN, EOS_ID, EOS_TOKEN, FormulaTokenizerMetadata, PAD_ID, PAD_TOKEN, UNK_ID,
    UNK_TOKEN,
};
pub use model_store::{
    default_model_store_dir, ensure_downloaded, sha256_file, verify_existing_file,
};
pub use ocr::config::{ModelConfig, RecognizeOptions, RecognizerConfig};
pub use ocr::pipeline::{
    config::{EngineConfig, GlobalConfig},
    rapid_ocr::{PipelineProviderResolutions, RapidOcr, RapidOcrEngine},
};
pub use ocr::types::{LineResult, RecognizeOutput, WordBox, WordInfo, WordType};
pub use output::html::{relative_image_name, render_output_report, render_report};
pub use output::json::{OcrJsonItem, to_output_items, to_output_json};
pub use output::markdown::to_output_markdown;
pub use output::visualize::draw_output;
pub use runtime::contracts::{ModelIoProbe, TensorSpec};
pub use runtime::provider::{ProviderResolution, ResolvedExecutionProvider};
pub use runtime::session::OrtSession;

pub type Quad = [[f32; 2]; 4];

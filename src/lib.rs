mod api;
mod cls;
mod config;
mod det;
mod error;
pub mod evaluation;
mod input;
mod model_registry;
mod model_store;
mod output;
mod pipeline;
mod rec;
mod runtime;
mod types;
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
    ColorOrder, LangCls, LangDet, LangRec, ModelConfig, ModelType, OcrVersion, ProviderPreference,
    RecImage, RecognizeOptions, RecognizerConfig, RuntimeBackend, RuntimeConfig, VisionBackend,
};
pub use error::{RapidOcrError, Result};
pub use model_store::{
    default_model_store_dir, ensure_downloaded, sha256_file, verify_existing_file,
};
pub use output::html::{relative_image_name, render_output_report, render_report};
pub use output::json::{OcrJsonItem, to_output_items, to_output_json};
pub use output::markdown::to_output_markdown;
pub use output::visualize::draw_output;
pub use pipeline::{
    config::{EngineConfig, GlobalConfig},
    rapid_ocr::{PipelineProviderResolutions, RapidOcr, RapidOcrEngine},
};
pub use runtime::provider::{ProviderResolution, ResolvedExecutionProvider};
pub use types::{LineResult, RecognizeOutput, WordBox, WordInfo, WordType};

pub type Quad = [[f32; 2]; 4];

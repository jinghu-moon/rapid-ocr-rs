//! Stable, application-neutral OCR contracts.

use crate::error::{RapidOcrError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Rgb8,
    Gray8,
    GrayAlpha8,
}
impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra8 | Self::Rgba8 => 4,
            Self::Rgb8 => 3,
            Self::Gray8 => 1,
            Self::GrayAlpha8 => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OwnedPixelBuffer {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub bottom_up: bool,
    pub data: Arc<[u8]>,
}
impl OwnedPixelBuffer {
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(RapidOcrError::InvalidInput(
                "pixel buffer width/height must be greater than zero".into(),
            ));
        }
        let row = (self.width as usize)
            .checked_mul(self.format.bytes_per_pixel())
            .ok_or_else(|| RapidOcrError::InvalidInput("pixel row size overflow".into()))?;
        if self.stride < row {
            return Err(RapidOcrError::InvalidInput(format!(
                "pixel stride {} is smaller than row size {}",
                self.stride, row
            )));
        }
        let required = self
            .stride
            .checked_mul(self.height as usize)
            .ok_or_else(|| RapidOcrError::InvalidInput("pixel buffer size overflow".into()))?;
        if self.data.len() < required {
            return Err(RapidOcrError::InvalidInput(format!(
                "pixel buffer has {} bytes, requires at least {}",
                self.data.len(),
                required
            )));
        }
        Ok(())
    }
    pub fn to_bgr(&self, roi: Option<RectU32>) -> Result<(Vec<u8>, ImageSize, (u32, u32))> {
        self.validate()?;
        let selected = roi.unwrap_or(RectU32 {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        });
        selected.validate_against(self.width, self.height)?;
        let channels = self.format.bytes_per_pixel();
        let row_bytes = selected.width as usize * 3;
        let mut out = vec![0; row_bytes * selected.height as usize];
        for y in 0..selected.height as usize {
            let sy = if self.bottom_up {
                self.height as usize - 1 - (selected.y as usize + y)
            } else {
                selected.y as usize + y
            };
            let start = sy * self.stride + selected.x as usize * channels;
            let src = &self.data[start..start + selected.width as usize * channels];
            let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..selected.width as usize {
                let s = &src[x * channels..(x + 1) * channels];
                let d = &mut dst[x * 3..x * 3 + 3];
                match self.format {
                    PixelFormat::Bgra8 => {
                        let a = s[3] as u16;
                        d[0] = ((s[0] as u16 * a + 255 * (255 - a)) / 255) as u8;
                        d[1] = ((s[1] as u16 * a + 255 * (255 - a)) / 255) as u8;
                        d[2] = ((s[2] as u16 * a + 255 * (255 - a)) / 255) as u8;
                    }
                    PixelFormat::Rgba8 => {
                        let a = s[3] as u16;
                        d[0] = ((s[2] as u16 * a + 255 * (255 - a)) / 255) as u8;
                        d[1] = ((s[1] as u16 * a + 255 * (255 - a)) / 255) as u8;
                        d[2] = ((s[0] as u16 * a + 255 * (255 - a)) / 255) as u8;
                    }
                    PixelFormat::Rgb8 => d.copy_from_slice(&[s[2], s[1], s[0]]),
                    PixelFormat::Gray8 => d.fill(s[0]),
                    PixelFormat::GrayAlpha8 => {
                        let a = s[1] as u16;
                        let value = ((s[0] as u16 * a + 255 * (255 - a)) / 255) as u8;
                        d.fill(value);
                    }
                }
            }
        }
        Ok((
            out,
            ImageSize {
                width: selected.width,
                height: selected.height,
            },
            (selected.x, selected.y),
        ))
    }
}

#[derive(Debug, Clone)]
pub enum ImageInput {
    Encoded(Arc<[u8]>),
    Pixels(OwnedPixelBuffer),
    File(PathBuf),
    Url(String),
    Image(crate::config::RecImage),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RectU32 {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl RectU32 {
    pub fn validate_against(self, w: u32, h: u32) -> Result<()> {
        let r = self
            .x
            .checked_add(self.width)
            .ok_or_else(|| RapidOcrError::InvalidInput("ROI x/width overflow".into()))?;
        let b = self
            .y
            .checked_add(self.height)
            .ok_or_else(|| RapidOcrError::InvalidInput("ROI y/height overflow".into()))?;
        if self.width == 0 || self.height == 0 || r > w || b > h {
            return Err(RapidOcrError::InvalidInput(format!(
                "ROI {self:?} is outside image {w}x{h}"
            )));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilePolicy {
    pub max_width: u32,
    pub max_height: u32,
    pub overlap: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EnhancementPolicy {
    #[default]
    None,
    ScreenAdaptive,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreprocessPolicy {
    /// Maximum number of decoded pixels accepted for any image input.
    pub max_decode_pixels: u64,
    /// Maximum encoded byte length accepted for any encoded image input.
    ///
    /// This bound applies uniformly to file, URL and caller-supplied encoded
    /// buffers so compressed payloads are rejected before any decode attempt.
    /// URL bodies are additionally streamed with a hard read cap derived from
    /// this value, so an absent or lying `Content-Length` cannot force an
    /// unbounded buffer. Decoded pixel buffers are covered separately by
    /// `max_decode_pixels`.
    pub max_encoded_bytes: u64,
    /// Maximum long side for engine preprocessing.
    ///
    /// `None` defers to `EngineConfig::global.max_side_len`; `Some` overrides
    /// that engine-level bound for the request.
    pub max_side: Option<u32>,
    /// Optional lower bound for text scale used during recognition.
    pub min_text_scale: Option<f32>,
    /// Optional tiling policy for segmented OCR.
    pub tile: Option<TilePolicy>,
    /// Optional image enhancement policy.
    pub enhance: EnhancementPolicy,
}
impl Default for PreprocessPolicy {
    fn default() -> Self {
        Self {
            max_decode_pixels: 24_000_000,
            max_encoded_bytes: 128 * 1024 * 1024,
            // `None` defers to `EngineConfig::global.max_side_len`.
            max_side: None,
            min_text_scale: None,
            tile: None,
            enhance: EnhancementPolicy::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassifierPolicy {
    Off,
    IfAvailable,
    Required,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierPlan {
    pub policy: ClassifierPolicy,
    pub apply_rotation: bool,
}
impl Default for ClassifierPlan {
    fn default() -> Self {
        Self {
            policy: ClassifierPolicy::IfAvailable,
            apply_rotation: true,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagePlan {
    pub detect: bool,
    pub classify: ClassifierPlan,
    pub recognize: bool,
}
impl Default for StagePlan {
    fn default() -> Self {
        Self {
            detect: true,
            classify: ClassifierPlan::default(),
            recognize: true,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WordOutputMode {
    Off,
    Words,
    Chars,
}
impl StagePlan {
    pub fn validate(self, words: WordOutputMode) -> Result<()> {
        if !self.detect
            && !self.recognize
            && matches!(self.classify.policy, ClassifierPolicy::Off)
            && matches!(words, WordOutputMode::Off)
        {
            return Err(RapidOcrError::InvalidInput(
                "at least one OCR stage must be enabled".into(),
            ));
        }
        if !self.recognize && !matches!(words, WordOutputMode::Off) {
            return Err(RapidOcrError::InvalidInput(
                "word output requires recognition stage".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct DetectionPolicy {
    pub box_thresh: Option<f32>,
    pub unclip_ratio: Option<f32>,
    pub text_score: Option<f32>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RecognitionPolicy {
    pub words: WordOutputMode,
}
impl Default for RecognitionPolicy {
    fn default() -> Self {
        Self {
            words: WordOutputMode::Off,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CoordinateSpace {
    #[default]
    Image,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct OutputPolicy {
    pub coordinate_space: CoordinateSpace,
}
impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            coordinate_space: CoordinateSpace::Image,
        }
    }
}
#[derive(Debug, Clone)]
pub struct OcrRequest {
    pub input: ImageInput,
    pub roi: Option<RectU32>,
    pub scale_hint: Option<f32>,
    pub stages: StagePlan,
    pub preprocess: PreprocessPolicy,
    pub detection: DetectionPolicy,
    pub recognition: RecognitionPolicy,
    pub output: OutputPolicy,
    /// 页面级公式路由；默认关闭，关闭时行为与不含公式功能时一致。
    pub formula: FormulaPolicy,
}
impl OcrRequest {
    pub fn validate(&self) -> Result<()> {
        self.stages.validate(self.recognition.words)?;
        self.formula.validate()?;
        if self.formula.enabled {
            if self.roi.is_some() {
                return Err(RapidOcrError::InvalidInput(
                    "formula routing does not support `roi`: formula regions are expressed in \
                     original image coordinates"
                        .into(),
                ));
            }
            if self.preprocess.tile.is_some() {
                return Err(RapidOcrError::InvalidInput(
                    "formula routing does not support tiled preprocessing".into(),
                ));
            }
        }
        if self.preprocess.max_decode_pixels == 0 {
            return Err(RapidOcrError::InvalidInput(
                "max_decode_pixels must be greater than zero".into(),
            ));
        }
        if self.preprocess.max_encoded_bytes == 0 {
            return Err(RapidOcrError::InvalidInput(
                "max_encoded_bytes must be greater than zero".into(),
            ));
        }
        if self.preprocess.max_side.is_some_and(|v| v == 0) {
            return Err(RapidOcrError::InvalidInput(
                "max_side must be greater than zero when set".into(),
            ));
        }
        if let Some(v) = self.preprocess.min_text_scale
            && (!v.is_finite() || v <= 0.0)
        {
            return Err(RapidOcrError::InvalidInput(
                "min_text_scale must be finite and greater than zero".into(),
            ));
        }
        if let Some(v) = self.scale_hint
            && (!v.is_finite() || v <= 0.0)
        {
            return Err(RapidOcrError::InvalidInput(
                "scale_hint must be finite and greater than zero".into(),
            ));
        }
        for (name, value) in [
            ("box_thresh", self.detection.box_thresh),
            ("text_score", self.detection.text_score),
        ] {
            if let Some(value) = value
                && (!value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                return Err(RapidOcrError::InvalidInput(format!(
                    "{name} must be finite and in [0,1]"
                )));
            }
        }
        if let Some(value) = self.detection.unclip_ratio
            && (!value.is_finite() || value <= 0.0)
        {
            return Err(RapidOcrError::InvalidInput(
                "unclip_ratio must be finite and greater than zero".into(),
            ));
        }
        if let Some(t) = self.preprocess.tile
            && (t.max_width == 0
                || t.max_height == 0
                || t.overlap >= t.max_width
                || t.overlap >= t.max_height)
        {
            return Err(RapidOcrError::InvalidInput(
                "tile dimensions must be positive and overlap must be smaller than dimensions"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    pub points: [[f32; 2]; 4],
}
impl Polygon {
    pub fn validate(self, size: ImageSize) -> Result<()> {
        for [x, y] in self.points {
            if !x.is_finite()
                || !y.is_finite()
                || x < 0.0
                || y < 0.0
                || x > size.width as f32
                || y > size.height as f32
            {
                return Err(RapidOcrError::InvalidInput(format!(
                    "polygon point ({x}, {y}) is outside image {:?}",
                    size
                )));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionSource {
    Detected { detector_index: usize },
    Input,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextOrientation {
    Deg0,
    Deg180,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionOutcome {
    pub score: f32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationOutcome {
    pub orientation: TextOrientation,
    pub score: f32,
    pub applied_rotation: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WordKind {
    Char,
    Token,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub score: f32,
    pub polygon: Polygon,
    pub kind: WordKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecognitionOutcome {
    pub text: String,
    pub score: f32,
    pub words: Option<Vec<OcrWord>>,
}

/// 区域语义：普通文本区域，或由公式模型识别的公式区域。
///
/// 公式区域**不携带** CTC 文本识别结果：公式模型没有可与 CTC 平行解释的
/// per-character 置信度，强行塞进 `RecognitionOutcome` 会伪造置信度语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegionKind {
    #[default]
    Text,
    Formula,
}

/// 公式区域识别结果（页面级输出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaOutcome {
    /// RapidDoc 兼容的 LaTeX（含 `fix_latex` 与已实现的 `ftfy` 步骤）。
    pub latex: String,
    pub eos_index: Option<usize>,
    pub truncated: bool,
    pub model_id: String,
    /// 原始 token 序列；仅在 `FormulaPolicy::include_token_ids` 时输出。
    pub token_ids: Option<Vec<i64>>,
}

/// 页面级公式路由策略。
///
/// `enabled = false` 时页面 OCR 行为与不含公式功能时完全一致；启用后：
///
/// 1. 用 `detector_path` 指定的公式检测模型（可选）在整页上检测公式区域，
///    并合并调用方通过 `input_regions` 显式声明的区域；
/// 2. 检测区域先做去重/包含消解，再按 `text_overlap_skip_ratio` 判断哪些
///    **文本检测框**落在公式区域内，这些框在送入普通 CTC 之前从图像上抹白，
///    因此不会产生 CTC 文本（真正“跳过 CTC”，而不是事后丢弃结果）；
/// 3. 公式区域使用原图（未抹白）裁剪后交给 `FormulaRecognizer` 识别；
/// 4. 结果是独立的 [`RegionKind::Formula`] 区域，保留 crop 坐标与模型信息。
///
/// 不支持 `roi` 与 `tile` 同时启用（坐标映射语义会分叉），此时返回结构化错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaPolicy {
    pub enabled: bool,
    /// 公式识别模型（`pp_formulanet_plus_m.onnx`）；`enabled` 时必填。
    pub model_path: Option<PathBuf>,
    /// 可选模型 SHA-256 校验。
    pub expected_model_sha256: Option<String>,
    /// 页面公式检测模型（`pix2text-mfd-1.5.onnx`）；为 `None` 时只处理 `input_regions`。
    pub detector_path: Option<PathBuf>,
    /// 公式检测置信度阈值。
    pub confidence_threshold: f32,
    /// 公式检测 NMS IoU 阈值。
    pub iou_threshold: f32,
    /// 每页最多保留的公式区域数。
    pub max_regions: usize,
    /// 候选区域面积占原图面积的最小比例，用于过滤明显误检。
    pub min_area_ratio: f32,
    /// 调用方显式声明的公式区域（原图像素坐标）。
    pub input_regions: Vec<Polygon>,
    /// 文本检测框被公式区域覆盖的比例达到该值时跳过其 CTC 识别。
    pub text_overlap_skip_ratio: f32,
    /// 是否在输出中保留原始 token IDs。
    pub include_token_ids: bool,
}

impl Default for FormulaPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: None,
            expected_model_sha256: None,
            detector_path: None,
            confidence_threshold: crate::formula::detect::DEFAULT_FORMULA_DETECT_CONFIDENCE,
            iou_threshold: crate::formula::detect::DEFAULT_FORMULA_DETECT_IOU,
            max_regions: 64,
            min_area_ratio: 0.000_05,
            input_regions: Vec::new(),
            text_overlap_skip_ratio: 0.6,
            include_token_ids: false,
        }
    }
}

impl FormulaPolicy {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.model_path.is_none() {
            return Err(RapidOcrError::InvalidInput(
                "formula policy requires `model_path` when enabled".into(),
            ));
        }
        for (name, value) in [
            ("confidence_threshold", self.confidence_threshold),
            ("iou_threshold", self.iou_threshold),
            ("min_area_ratio", self.min_area_ratio),
            ("text_overlap_skip_ratio", self.text_overlap_skip_ratio),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(RapidOcrError::InvalidInput(format!(
                    "formula {name} must be finite and in [0,1]"
                )));
            }
        }
        if self.max_regions == 0 {
            return Err(RapidOcrError::InvalidInput(
                "formula max_regions must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrRegion {
    pub source: RegionSource,
    /// 区域语义；默认为普通文本区域。
    #[serde(default)]
    pub kind: RegionKind,
    pub polygon: Option<Polygon>,
    pub detection: Option<DetectionOutcome>,
    pub classification: Option<ClassificationOutcome>,
    pub recognition: Option<RecognitionOutcome>,
    /// 公式区域结果；仅当 `kind == RegionKind::Formula` 时为 `Some`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formula: Option<FormulaOutcome>,
}

impl OcrRegion {
    /// 构造普通文本区域（公式字段为空）。
    pub fn text(
        source: RegionSource,
        polygon: Option<Polygon>,
        detection: Option<DetectionOutcome>,
        classification: Option<ClassificationOutcome>,
        recognition: Option<RecognitionOutcome>,
    ) -> Self {
        Self {
            source,
            kind: RegionKind::Text,
            polygon,
            detection,
            classification,
            recognition,
            formula: None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StageState {
    #[default]
    Disabled,
    SkippedNoInput,
    SkippedUnavailable,
    Completed {
        items: usize,
    },
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StageTiming {
    pub preprocess_ms: f32,
    pub infer_ms: f32,
    pub postprocess_ms: f32,
}
impl StageTiming {
    pub fn total_ms(self) -> f32 {
        self.preprocess_ms + self.infer_ms + self.postprocess_ms
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StageReport {
    pub state: StageState,
    pub timing: Option<StageTiming>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputTimings {
    pub decode_ms: Option<f32>,
    pub resize_ms: Option<f32>,
    pub crop_ms: Option<f32>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageReports {
    pub input: InputTimings,
    pub detector: StageReport,
    pub classifier: StageReport,
    pub recognizer: StageReport,
    /// 页面级公式路由阶段；未启用时为 `Disabled`。
    #[serde(default)]
    pub formula: StageReport,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OcrTimings {
    pub decode_ms: f32,
    pub resize_ms: f32,
    pub crop_ms: f32,
    pub preprocess_ms: f32,
    pub detector_preprocess_ms: f32,
    pub detector_infer_ms: f32,
    pub detector_postprocess_ms: f32,
    pub detect_ms: f32,
    pub classifier_preprocess_ms: f32,
    pub classifier_infer_ms: f32,
    pub classifier_postprocess_ms: f32,
    pub classify_ms: f32,
    pub recognizer_preprocess_ms: f32,
    pub recognizer_infer_ms: f32,
    pub recognizer_postprocess_ms: f32,
    pub recognize_ms: f32,
    /// 页面级公式路由耗时（检测 + 裁剪 + 公式识别）；未启用时为 0。
    #[serde(default)]
    pub formula_ms: f32,
    pub postprocess_ms: f32,
    pub total_ms: f32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineInfo {
    pub model_id: String,
    pub provider: ProviderInfo,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub original_size: ImageSize,
    pub processed_size: ImageSize,
    pub coordinate_space: CoordinateSpace,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrOutput {
    pub schema_version: u32,
    pub image: ImageInfo,
    pub stages: StageReports,
    pub regions: Vec<OcrRegion>,
    pub timings: OcrTimings,
    pub engine: EngineInfo,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOrder {
    Reading,
    Detection,
}
impl OcrOutput {
    /// 区域总数（文本 + 公式）。
    ///
    /// 公式区域也是识别结果：只有公式的页面不是空页面。需要文本数量时使用
    /// [`OcrOutput::text_len`]。
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// 带 CTC 文本结果的区域数。
    pub fn text_len(&self) -> usize {
        self.regions
            .iter()
            .filter(|region| region.recognition.is_some())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn reading_order(&self) -> Vec<usize> {
        self.reading_order_groups().into_iter().flatten().collect()
    }

    pub(crate) fn reading_order_groups(&self) -> Vec<Vec<usize>> {
        let mut boxes = Vec::with_capacity(self.regions.len());
        let mut without_polygon = Vec::new();
        for (id, region) in self.regions.iter().enumerate() {
            let Some(polygon) = region.polygon else {
                without_polygon.push(id);
                continue;
            };
            let Some(reading_box) = ReadingBox::from_polygon(id, polygon) else {
                without_polygon.push(id);
                continue;
            };
            boxes.push(reading_box);
        }
        let mut groups = order_reading_groups(&boxes);
        groups.extend(without_polygon.into_iter().map(|id| vec![id]));
        groups
    }
    pub fn plain_text(&self, order: TextOrder) -> String {
        let ids = match order {
            TextOrder::Reading => self.reading_order(),
            TextOrder::Detection => (0..self.regions.len()).collect(),
        };
        ids.into_iter()
            .filter_map(|i| self.regions[i].recognition.as_ref())
            .map(|v| v.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 按阅读顺序返回公式区域 (region id, latex)。
    pub fn formula_latex(&self, order: TextOrder) -> Vec<(usize, &str)> {
        let ids = match order {
            TextOrder::Reading => self.reading_order(),
            TextOrder::Detection => (0..self.regions.len()).collect(),
        };
        ids.into_iter()
            .filter_map(|id| {
                let region = self.regions.get(id)?;
                let formula = region.formula.as_ref()?;
                Some((id, formula.latex.as_str()))
            })
            .collect()
    }

    /// 公式区域数量。
    pub fn formula_count(&self) -> usize {
        self.regions
            .iter()
            .filter(|region| region.kind == RegionKind::Formula)
            .count()
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(RapidOcrError::InvalidInput(format!(
                "unsupported OCR schema version {}",
                self.schema_version
            )));
        }
        for region in &self.regions {
            match region.kind {
                RegionKind::Formula => {
                    if region.formula.is_none() {
                        return Err(RapidOcrError::InvalidInput(
                            "formula region must carry a formula outcome".into(),
                        ));
                    }
                    if region.recognition.is_some() {
                        return Err(RapidOcrError::InvalidInput(
                            "formula region must not carry a CTC recognition outcome".into(),
                        ));
                    }
                }
                RegionKind::Text => {
                    if region.formula.is_some() {
                        return Err(RapidOcrError::InvalidInput(
                            "text region must not carry a formula outcome".into(),
                        ));
                    }
                }
            }
            if let Some(polygon) = region.polygon {
                polygon.validate(self.image.original_size)?;
            }
            if let Some(detection) = &region.detection
                && (!detection.score.is_finite() || !(0.0..=1.0).contains(&detection.score))
            {
                return Err(RapidOcrError::InvalidInput(
                    "detection score must be finite and in [0,1]".into(),
                ));
            }
            if let Some(classification) = &region.classification
                && (!classification.score.is_finite()
                    || !(0.0..=1.0).contains(&classification.score))
            {
                return Err(RapidOcrError::InvalidInput(
                    "classification score must be finite and in [0,1]".into(),
                ));
            }
            if let Some(recognition) = &region.recognition {
                if !recognition.score.is_finite() || !(0.0..=1.0).contains(&recognition.score) {
                    return Err(RapidOcrError::InvalidInput(
                        "recognition score must be finite and in [0,1]".into(),
                    ));
                }
                if let Some(words) = &recognition.words {
                    for word in words {
                        if !word.score.is_finite() || !(0.0..=1.0).contains(&word.score) {
                            return Err(RapidOcrError::InvalidInput(
                                "word score must be finite and in [0,1]".into(),
                            ));
                        }
                        word.polygon.validate(self.image.original_size)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadingBox {
    id: usize,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl ReadingBox {
    fn from_polygon(id: usize, polygon: Polygon) -> Option<Self> {
        let mut x0 = f32::INFINITY;
        let mut y0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        let mut y1 = f32::NEG_INFINITY;
        for [x, y] in polygon.points {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Some(Self { id, x0, y0, x1, y1 })
    }

    fn height(self) -> f32 {
        (self.y1 - self.y0).max(0.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct Gap {
    start: f32,
    end: f32,
}

impl Gap {
    fn extent(self) -> f32 {
        (self.end - self.start).max(0.0)
    }

    fn center(self) -> f32 {
        (self.start + self.end) * 0.5
    }
}

#[cfg(test)]
fn order_reading_boxes(boxes: &[ReadingBox]) -> Vec<usize> {
    order_reading_groups(boxes).into_iter().flatten().collect()
}

fn order_reading_groups(boxes: &[ReadingBox]) -> Vec<Vec<usize>> {
    let mut groups = Vec::with_capacity(boxes.len());
    append_reading_order(boxes, &mut groups);
    groups
}

fn append_reading_order(boxes: &[ReadingBox], groups: &mut Vec<Vec<usize>>) {
    if boxes.is_empty() {
        return;
    }
    if boxes.len() == 1 {
        groups.push(vec![boxes[0].id]);
        return;
    }

    let median_height = median_box_height(boxes);
    let vertical_gap = largest_vertical_gap(boxes);
    let horizontal_gap = largest_horizontal_gap(boxes);

    // Large full-width horizontal whitespace is a section break. Split it
    // before column detection so independent top/bottom bands are not
    // interleaved by global x-coordinate columns.
    if let Some(gap) = horizontal_gap
        && gap.extent() >= median_height * SECTION_BREAK_MEDIAN_HEIGHT_FACTOR
        && append_horizontal_split(boxes, gap, groups)
    {
        return;
    }

    // Otherwise vertical whitespace is the strongest signal for columns.
    // Split on it first so two-column pages are read column-by-column rather
    // than by rows.
    if let Some(gap) = vertical_gap {
        let threshold = (median_height * 0.5).max(2.0);
        if gap.extent() >= threshold {
            let split = gap.center();
            let left: Vec<ReadingBox> = boxes.iter().copied().filter(|b| b.x1 <= split).collect();
            let right: Vec<ReadingBox> = boxes.iter().copied().filter(|b| b.x0 >= split).collect();
            if !left.is_empty() && !right.is_empty() && left.len() + right.len() == boxes.len() {
                append_reading_order(&left, groups);
                append_reading_order(&right, groups);
                return;
            }
        }
    }

    // A smaller horizontal gap still separates headings/footers and
    // single-column lines.
    if let Some(gap) = horizontal_gap
        && gap.extent() >= 1.0
        && append_horizontal_split(boxes, gap, groups)
    {
        return;
    }

    append_reading_order_by_lines(boxes, groups);
}

fn append_horizontal_split(boxes: &[ReadingBox], gap: Gap, groups: &mut Vec<Vec<usize>>) -> bool {
    let split = gap.center();
    let top: Vec<ReadingBox> = boxes.iter().copied().filter(|b| b.y1 <= split).collect();
    let bottom: Vec<ReadingBox> = boxes.iter().copied().filter(|b| b.y0 >= split).collect();
    if top.is_empty() || bottom.is_empty() || top.len() + bottom.len() != boxes.len() {
        return false;
    }
    append_reading_order(&top, groups);
    append_reading_order(&bottom, groups);
    true
}

fn append_reading_order_by_lines(boxes: &[ReadingBox], groups: &mut Vec<Vec<usize>>) {
    let mut remaining = boxes.to_vec();
    remaining.sort_by(|a, b| {
        a.y0.total_cmp(&b.y0)
            .then_with(|| a.x0.total_cmp(&b.x0))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut lines: Vec<Vec<ReadingBox>> = Vec::new();
    for item in remaining {
        let mut best_line: Option<(usize, f32)> = None;
        for (index, line) in lines.iter().enumerate() {
            let line_top = line.iter().map(|b| b.y0).fold(f32::INFINITY, f32::min);
            let line_bottom = line.iter().map(|b| b.y1).fold(f32::NEG_INFINITY, f32::max);
            let line_height = (line_bottom - line_top).max(0.0);
            let overlap = (line_bottom.min(item.y1) - line_top.max(item.y0)).max(0.0);
            let min_height = line_height.min(item.height()).max(0.0001);
            if overlap / min_height >= 0.5
                && best_line.is_none_or(|(_, best_overlap)| overlap > best_overlap)
            {
                best_line = Some((index, overlap));
            }
        }
        if let Some((index, _)) = best_line {
            lines[index].push(item);
        } else {
            lines.push(vec![item]);
        }
    }

    lines.sort_by(|a, b| {
        let a_top = a.iter().map(|b| b.y0).fold(f32::INFINITY, f32::min);
        let b_top = b.iter().map(|b| b.y0).fold(f32::INFINITY, f32::min);
        a_top.total_cmp(&b_top).then_with(|| {
            let a_left = a.iter().map(|b| b.x0).fold(f32::INFINITY, f32::min);
            let b_left = b.iter().map(|b| b.x0).fold(f32::INFINITY, f32::min);
            a_left.total_cmp(&b_left)
        })
    });

    for mut line in lines {
        line.sort_by(|a, b| {
            a.x0.total_cmp(&b.x0)
                .then_with(|| a.y0.total_cmp(&b.y0))
                .then_with(|| a.id.cmp(&b.id))
        });
        groups.push(line.into_iter().map(|b| b.id).collect());
    }
}

const SECTION_BREAK_MEDIAN_HEIGHT_FACTOR: f32 = 1.5;

fn median_box_height(boxes: &[ReadingBox]) -> f32 {
    let mut heights: Vec<f32> = boxes
        .iter()
        .map(|b| b.height())
        .filter(|height| height.is_finite() && *height > 0.0)
        .collect();
    if heights.is_empty() {
        return 1.0;
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    heights[heights.len() / 2]
}

fn largest_vertical_gap(boxes: &[ReadingBox]) -> Option<Gap> {
    largest_gap(boxes.iter().map(|b| (b.x0, b.x1)))
}

fn largest_horizontal_gap(boxes: &[ReadingBox]) -> Option<Gap> {
    largest_gap(boxes.iter().map(|b| (b.y0, b.y1)))
}

fn largest_gap<I>(intervals: I) -> Option<Gap>
where
    I: IntoIterator<Item = (f32, f32)>,
{
    let mut intervals: Vec<(f32, f32)> = intervals
        .into_iter()
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
        .collect();
    if intervals.len() < 2 {
        return None;
    }
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut merged: Vec<(f32, f32)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }

    merged
        .windows(2)
        .map(|pair| Gap {
            start: pair[0].1,
            end: pair[1].0,
        })
        .filter(|gap| gap.end > gap.start)
        .max_by(|a, b| {
            a.extent()
                .total_cmp(&b.extent())
                .then_with(|| b.start.total_cmp(&a.start))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedProvider {
    Cpu,
    DirectMl,
    Cuda,
    Cann,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderPreference {
    Cpu,
    DirectMl { device_id: usize },
    Cuda { device_id: usize },
    Cann { device_id: usize },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResolutionInfo {
    pub requested: ProviderPreference,
    pub resolved: ResolvedProvider,
    pub fallback_to_cpu: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub detector: ProviderResolutionInfo,
    pub classifier: Option<ProviderResolutionInfo>,
    pub recognizer: ProviderResolutionInfo,
}
pub trait OcrEngine {
    fn model_id(&self) -> &str;
    fn provider(&self) -> ProviderInfo;
    fn recognize(&mut self, request: OcrRequest) -> Result<OcrOutput>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArtifact {
    pub file_name: String,
    pub sha256: String,
    pub source_url: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub id: String,
    pub family: String,
    pub version: String,
    pub languages: Vec<String>,
    pub detector: ModelArtifact,
    pub recognizer: ModelArtifact,
    pub dictionary: ModelArtifact,
    pub classifier: Option<ModelArtifact>,
}
impl ModelManifest {
    pub fn validate_files(&self, root: impl AsRef<std::path::Path>) -> Result<()> {
        let root = root.as_ref();
        let mut all = vec![&self.detector, &self.recognizer, &self.dictionary];
        if let Some(v) = &self.classifier {
            all.push(v);
        }
        for a in all {
            let p = std::path::Path::new(&a.file_name);
            if p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir) {
                return Err(RapidOcrError::ModelResolve(format!(
                    "model artifact path escapes manifest root: {}",
                    a.file_name
                )));
            }
            let path = root.join(&a.file_name);
            let actual = crate::model_store::sha256_file(&path)?;
            if !actual.eq_ignore_ascii_case(&a.sha256) {
                return Err(RapidOcrError::HashMismatch {
                    path,
                    expected: a.sha256.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
pub enum ModelSource {
    Path(std::path::PathBuf),
    Bytes(Arc<[u8]>),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_plan() {
        let p = StagePlan {
            detect: false,
            classify: ClassifierPlan {
                policy: ClassifierPolicy::Off,
                apply_rotation: false,
            },
            recognize: false,
        };
        assert!(p.validate(WordOutputMode::Off).is_err());
    }
    #[test]
    fn words_require_recognition() {
        let p = StagePlan {
            recognize: false,
            ..StagePlan::default()
        };
        assert!(p.validate(WordOutputMode::Words).is_err());
    }

    #[test]
    fn stage_plan_accepts_all_non_empty_combinations() {
        let classifier = ClassifierPlan {
            policy: ClassifierPolicy::IfAvailable,
            apply_rotation: true,
        };
        for (detect, classify, recognize) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let plan = StagePlan {
                detect,
                classify: if classify {
                    classifier
                } else {
                    ClassifierPlan {
                        policy: ClassifierPolicy::Off,
                        apply_rotation: false,
                    }
                },
                recognize,
            };
            assert!(
                plan.validate(WordOutputMode::Off).is_ok(),
                "invalid combination: {detect}/{classify}/{recognize}"
            );
        }
    }

    #[test]
    fn polygon_rejects_non_finite_or_out_of_bounds_points() {
        let size = ImageSize {
            width: 100,
            height: 80,
        };
        assert!(
            Polygon {
                points: [[0.0, 0.0], [100.0, 0.0], [100.0, 80.0], [0.0, 80.0]]
            }
            .validate(size)
            .is_ok()
        );
        assert!(
            Polygon {
                points: [[f32::NAN, 0.0]; 4]
            }
            .validate(size)
            .is_err()
        );
        assert!(
            Polygon {
                points: [[101.0, 0.0]; 4]
            }
            .validate(size)
            .is_err()
        );
    }

    #[test]
    fn request_validation_rejects_invalid_policy_values() {
        let request = OcrRequest {
            input: ImageInput::Encoded(Arc::from([] as [u8; 0])),
            roi: None,
            scale_hint: None,
            stages: StagePlan::default(),
            preprocess: PreprocessPolicy {
                min_text_scale: Some(f32::NAN),
                ..PreprocessPolicy::default()
            },
            detection: DetectionPolicy {
                text_score: Some(1.5),
                ..DetectionPolicy::default()
            },
            recognition: RecognitionPolicy::default(),
            output: OutputPolicy::default(),
            formula: FormulaPolicy::default(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn gray_alpha_pixels_are_composited() {
        let pixels = OwnedPixelBuffer {
            width: 1,
            height: 1,
            stride: 2,
            format: PixelFormat::GrayAlpha8,
            bottom_up: false,
            data: Arc::from([0_u8, 0_u8]),
        };
        let (bgr, _, _) = pixels.to_bgr(None).expect("valid gray alpha");
        assert_eq!(bgr, vec![255, 255, 255]);
    }

    #[test]
    fn default_preprocess_policy_defers_max_side_to_engine_config() {
        let policy = PreprocessPolicy::default();
        assert_eq!(policy.max_side, None);
        assert_eq!(policy.max_encoded_bytes, 128 * 1024 * 1024);
    }

    fn reading_box(id: usize, x0: f32, y0: f32, x1: f32, y1: f32) -> ReadingBox {
        ReadingBox { id, x0, y0, x1, y1 }
    }

    #[test]
    fn reading_order_uses_columns_before_rows() {
        let boxes = vec![
            reading_box(0, 10.0, 10.0, 60.0, 30.0),
            reading_box(1, 10.0, 40.0, 60.0, 60.0),
            reading_box(2, 100.0, 10.0, 150.0, 30.0),
            reading_box(3, 100.0, 40.0, 150.0, 60.0),
        ];
        assert_eq!(order_reading_boxes(&boxes), vec![0, 1, 2, 3]);
    }

    #[test]
    fn reading_order_handles_full_width_heading_then_columns() {
        let boxes = vec![
            reading_box(0, 10.0, 0.0, 190.0, 20.0),
            reading_box(1, 10.0, 30.0, 80.0, 50.0),
            reading_box(2, 10.0, 60.0, 80.0, 80.0),
            reading_box(3, 110.0, 30.0, 190.0, 50.0),
            reading_box(4, 110.0, 60.0, 190.0, 80.0),
        ];
        assert_eq!(order_reading_boxes(&boxes), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn reading_order_handles_three_columns() {
        let boxes = vec![
            reading_box(0, 0.0, 10.0, 30.0, 30.0),
            reading_box(1, 0.0, 40.0, 30.0, 60.0),
            reading_box(2, 50.0, 10.0, 80.0, 30.0),
            reading_box(3, 50.0, 40.0, 80.0, 60.0),
            reading_box(4, 100.0, 10.0, 130.0, 30.0),
            reading_box(5, 100.0, 40.0, 130.0, 60.0),
        ];
        assert_eq!(order_reading_boxes(&boxes), vec![0, 1, 2, 3, 4, 5]);
    }

    #[derive(serde::Deserialize)]
    struct RealLayoutFixture {
        source: String,
        boxes: Vec<RealLayoutBox>,
        expected: Vec<usize>,
    }

    #[derive(serde::Deserialize)]
    struct RealLayoutBox {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    }

    #[test]
    fn reading_order_matches_real_10_columns_layout() {
        let fixture: RealLayoutFixture = serde_json::from_str(include_str!(
            "../tests/fixtures/reading_order_10_columns.json"
        ))
        .expect("real layout fixture should parse");
        let boxes: Vec<ReadingBox> = fixture
            .boxes
            .iter()
            .enumerate()
            .map(|(id, b)| reading_box(id, b.x0, b.y0, b.x1, b.y1))
            .collect();
        assert_eq!(
            order_reading_boxes(&boxes),
            fixture.expected,
            "reading order mismatch for {}",
            fixture.source
        );
    }
}

//! 页面级数学公式检测（`pix2text-mfd-1.5`，Ultralytics YOLO11m detect 导出）。
//!
//! 该模块自成一体：只在共享 runtime 之上固化本模型的 IO 契约与前/后处理，不依赖
//! `formula::recognizer`（识别）或 `ocr`（普通 OCR）。
//!
//! 模型事实（已实测，见 `tools/formula_detect_reference.py`）：
//!
//! - 输入 `images`：`FLOAT32 [batch, 3, height, width]`，全部动态；
//! - 输出 `output0`：`FLOAT32 [batch, 6, A]`，`A` 为 stride 8/16/32 的 anchor 总数
//!   （768x768 时 `96*96 + 48*48 + 24*24 = 12096`）；
//! - 导出的计算图**已经包含** DFL/`dist2bbox`/sigmoid 头，因此 6 个通道是
//!   `[cx, cy, w, h, score_c0, score_c1]`，单位是 **letterboxed 输入像素**，既不归一化
//!   也不是 stride 单位；`cx/cy` 已经是绝对像素坐标，**不得**再加 anchor 偏移；
//! - metadata `names` 给出类别名（`{0: 'embedding', 1: 'isolated'}`）。
//!
//! 预处理与 Ultralytics `LetterBox` 对齐：保持宽高比缩放到 768x768，`114` 填充居中，
//! `left = round(dw - 0.1)`、`top = round(dh - 0.1)`；插值使用 `image` crate 的
//! `FilterType::Triangle`（双线性），与 PIL `BILINEAR` 属于同一族但不是位一致的实现，
//! 因此像素级会有 1 LSB 级别的差异，检测框坐标同样只承诺浮点级别的一致
//! （实测参考实现与 Rust 的最大坐标差 < 1e-3 像素，见 `rust_scale_back_*` 测试）。
//!
//! **通道顺序**：Ultralytics 从 cv2 读到的是 BGR 帧，推理前才 `BGR -> RGB`，所以模型
//! 期待的就是 RGB；我们的 [`image::DynamicImage`] 本来就是 RGB，因此 [`letterbox`]
//! **不做**通道交换，只按 `R, G, B` 依次写入 NCHW 的三个平面。该结论由
//! `letterbox_channel_order_is_rgb` 测试锁定。

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use image::{DynamicImage, GenericImageView, imageops::FilterType};
use ndarray::{Array4, ArrayView2, ArrayView4, s};
use ort::value::TensorElementType;
use serde::{Deserialize, Serialize};

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    runtime::{
        contracts::{
            ModelIoProbe, TensorSpec, require_fixed_dim, require_single_input,
            require_single_output, require_tensor,
        },
        provider::{ProviderResolution, require_requested_provider},
        session::OrtSession,
    },
};

/// 网络输入边长（`imgsz=[768, 768]`）。
pub const FORMULA_DETECT_INPUT_SIZE: usize = 768;
/// 默认置信度阈值。
pub const DEFAULT_FORMULA_DETECT_CONFIDENCE: f32 = 0.25;
/// 默认 NMS IoU 阈值。
pub const DEFAULT_FORMULA_DETECT_IOU: f32 = 0.7;
/// 每张图默认最多保留的检测框数量。
pub const DEFAULT_FORMULA_DETECT_MAX_DETECTIONS: usize = 300;

/// 输入 rank 契约。
const FORMULA_DETECT_INPUT_RANK: usize = 4;
/// 输出 rank 契约（`[batch, 4 + nc, anchors]`）。
const FORMULA_DETECT_OUTPUT_RANK: usize = 3;
/// letterbox 填充值（`114/255`）。
const PAD_VALUE: f32 = 114.0 / 255.0;
/// Ultralytics `max_wh`：把类别偏移到互不重叠的坐标区间，使 NMS 变成“类别感知”。
const CLASS_OFFSET: f32 = 7680.0;

/// 一个检测到的公式区域，坐标为**原图像素**，从左上角开始顺时针。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormulaBox {
    pub polygon: [[f32; 2]; 4],
    pub score: f32,
    pub class_id: usize,
    pub class_name: String,
}

/// [`FormulaDetector::detect`] 的阈值与上限。
#[derive(Debug, Clone, Copy)]
pub struct FormulaDetectOptions {
    pub confidence_threshold: f32,
    pub iou_threshold: f32,
    pub max_detections: usize,
}

impl Default for FormulaDetectOptions {
    fn default() -> Self {
        Self {
            confidence_threshold: DEFAULT_FORMULA_DETECT_CONFIDENCE,
            iou_threshold: DEFAULT_FORMULA_DETECT_IOU,
            max_detections: DEFAULT_FORMULA_DETECT_MAX_DETECTIONS,
        }
    }
}

/// 页面级公式检测器。
#[derive(Debug)]
pub struct FormulaDetector {
    inner: OrtSession,
    /// 长度即类别数 `nc`；构造时已经与模型输出通道数对齐。
    class_names: Vec<String>,
    model_path: PathBuf,
}

impl FormulaDetector {
    /// 打开模型、校验 IO 契约与 provider 契约，不执行推理。
    ///
    /// provider 语义与 `formula::session::FormulaSession` 一致：请求了加速器却只拿到
    /// CPU 回退时必须失败，不能静默降级。
    pub fn from_model(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        if !model_path.is_file() {
            return Err(RapidOcrError::FileNotFound(model_path.to_path_buf()));
        }
        let inner = OrtSession::open_unchecked(model_path, runtime_cfg)?;
        // 公式域契约：请求了加速器却只拿到 CPU 回退时必须失败，不能静默降级。
        require_requested_provider(inner.provider_resolution())?;
        let probe = inner.probe_io()?;
        let contract = validate_detect_contract(&probe, model_path)?;
        let names_raw = inner.metadata_custom("names")?;
        let class_names =
            resolve_class_names(names_raw.as_deref(), contract.num_classes, model_path)?;
        Ok(Self {
            inner,
            class_names,
            model_path: model_path.to_path_buf(),
        })
    }

    /// 类别名；长度等于模型输出的类别数。
    pub fn class_names(&self) -> &[String] {
        &self.class_names
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.inner.provider_resolution()
    }

    /// 检测单张图片。
    pub fn detect(
        &mut self,
        image: &DynamicImage,
        options: &FormulaDetectOptions,
    ) -> Result<Vec<FormulaBox>> {
        validate_detect_options(options)?;
        let letterboxed = letterbox(image)?;
        let original = (image.width(), image.height());
        let mut results = self.run_letterboxed(
            letterboxed.tensor.view(),
            std::slice::from_ref(&letterboxed.meta),
            std::slice::from_ref(&original),
            options,
        )?;
        Ok(results.pop().unwrap_or_default())
    }

    /// 批量检测；`results[i]` 恒对应 `images[i]`，空批次返回空 `Vec`。
    pub fn detect_batch(
        &mut self,
        images: &[DynamicImage],
        options: &FormulaDetectOptions,
    ) -> Result<Vec<Vec<FormulaBox>>> {
        validate_detect_options(options)?;
        let Some(prepared) = prepare_batch(images)? else {
            return Ok(Vec::new());
        };
        self.run_letterboxed(
            prepared.tensor.view(),
            &prepared.metas,
            &prepared.originals,
            options,
        )
    }

    /// 推理 + 解码的唯一出口：输入张量必须已经是 `[N, 3, 768, 768]`。
    fn run_letterboxed(
        &mut self,
        tensor: ArrayView4<'_, f32>,
        metas: &[LetterboxMeta],
        originals: &[(u32, u32)],
        options: &FormulaDetectOptions,
    ) -> Result<Vec<Vec<FormulaBox>>> {
        let Self {
            inner,
            class_names,
            model_path,
        } = self;
        debug_assert_eq!(tensor.shape()[1], 3);
        debug_assert_eq!(tensor.shape()[2], FORMULA_DETECT_INPUT_SIZE);
        debug_assert_eq!(tensor.shape()[3], FORMULA_DETECT_INPUT_SIZE);
        debug_assert_eq!(tensor.shape()[0], metas.len());
        debug_assert_eq!(metas.len(), originals.len());

        let expected_channels = 4 + class_names.len();
        let label = model_path.display().to_string();
        inner.run_array3_view_with(tensor, |output| {
            let shape = output.shape();
            if shape[0] != metas.len() {
                return Err(RapidOcrError::Decode(format!(
                    "formula detect model {label} returned batch {} for {} inputs",
                    shape[0],
                    metas.len()
                )));
            }
            if shape[1] != expected_channels {
                return Err(RapidOcrError::Decode(format!(
                    "formula detect model {label} returned {} output channels, expected \
                     {expected_channels} (4 box channels + {} classes)",
                    shape[1],
                    class_names.len()
                )));
            }
            let mut results = Vec::with_capacity(metas.len());
            for (index, meta) in metas.iter().enumerate() {
                results.push(decode_output(
                    output.slice(s![index, .., ..]),
                    meta,
                    originals[index],
                    options,
                    class_names,
                ));
            }
            Ok(results)
        })
    }
}

/// 通过校验的检测模型签名。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectContract {
    /// 输出类别数；`None` 表示输出通道维是动态值，只能靠 metadata 推断。
    num_classes: Option<usize>,
}

/// 校验检测模型 IO 契约；探针与运行时共用同一份实现。
fn validate_detect_contract(probe: &ModelIoProbe, model_path: &Path) -> Result<DetectContract> {
    let input = require_single_input(probe, model_path, "formula detect")?;
    require_tensor(
        input,
        "input",
        FORMULA_DETECT_INPUT_RANK,
        TensorElementType::Float32,
        model_path,
    )?;
    require_fixed_dim(input, "input", 1, 3, model_path)?;
    // 空间维允许动态（本模块始终喂 768），但固定成别的尺寸必须拒绝：那样的模型永远
    // 收不到它能用的输入。
    require_dynamic_or_fixed(
        input,
        "input",
        2,
        FORMULA_DETECT_INPUT_SIZE as i64,
        model_path,
    )?;
    require_dynamic_or_fixed(
        input,
        "input",
        3,
        FORMULA_DETECT_INPUT_SIZE as i64,
        model_path,
    )?;

    let output = require_single_output(probe, model_path, "formula detect")?;
    require_tensor(
        output,
        "output",
        FORMULA_DETECT_OUTPUT_RANK,
        TensorElementType::Float32,
        model_path,
    )?;
    let channels = output.dims.get(1).copied().unwrap_or(-1);
    let num_classes = if channels > 4 {
        Some((channels - 4) as usize)
    } else if channels < 0 {
        None
    } else {
        return Err(RapidOcrError::Config(format!(
            "formula detect model output `{}` must expose 4 box channels plus at least one \
             class score, got {channels} channels (model={})",
            output.name,
            model_path.display()
        )));
    };
    Ok(DetectContract { num_classes })
}

/// `dim` 必须是 `fixed` 或动态（`-1`）；其它固定值直接拒绝。
fn require_dynamic_or_fixed(
    spec: &TensorSpec,
    io_kind: &str,
    dim_index: usize,
    fixed: i64,
    model_path: &Path,
) -> Result<()> {
    let actual = spec.dims.get(dim_index).copied().unwrap_or(-1);
    if actual == -1 || actual == fixed {
        return Ok(());
    }
    Err(RapidOcrError::Config(format!(
        "formula detect model {io_kind} `{}` dim {dim_index} must be dynamic or {fixed}, got \
         {actual} (dims={:?}, model={})",
        spec.name,
        spec.dims,
        model_path.display()
    )))
}

/// 解析 Ultralytics `names` metadata（`{0: 'embedding', 1: 'isolated'}`）。
///
/// 只支持这一种形态：花括号包裹、`索引: 名字`、逗号分隔、名字可用单/双引号包裹。
/// 任何不符合的地方都返回空 `Vec`，由 [`resolve_class_names`] 回退到 `class_{i}`。
fn parse_names_metadata(raw: &str) -> Vec<String> {
    let Some(body) = raw
        .trim()
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return Vec::new();
    };
    let mut entries: Vec<(usize, String)> = Vec::new();
    for entry in body.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((key, value)) = entry.split_once(':') else {
            return Vec::new();
        };
        let Ok(index) = key.trim().parse::<usize>() else {
            return Vec::new();
        };
        let name = value.trim().trim_matches(|c| c == '\'' || c == '"').trim();
        if name.is_empty() {
            return Vec::new();
        }
        entries.push((index, name.to_string()));
    }
    entries.sort_by_key(|(index, _)| *index);
    // 索引必须从 0 开始连续，否则说明 metadata 与输出通道对不上。
    if entries
        .iter()
        .enumerate()
        .any(|(expected, (index, _))| *index != expected)
    {
        return Vec::new();
    }
    entries.into_iter().map(|(_, name)| name).collect()
}

/// 决定最终的类别名列表：metadata 可用且数量与输出一致时用它，否则用 `class_{i}`。
///
/// 返回值的长度就是类别数，因此 [`FormulaDetector`] 中恒有 `class_names.len() == nc`。
fn resolve_class_names(
    raw: Option<&str>,
    num_classes: Option<usize>,
    model_path: &Path,
) -> Result<Vec<String>> {
    let parsed = raw.map(parse_names_metadata).unwrap_or_default();
    match num_classes {
        Some(count) => {
            if parsed.len() == count {
                Ok(parsed)
            } else {
                Ok((0..count).map(|index| format!("class_{index}")).collect())
            }
        }
        None => {
            if parsed.is_empty() {
                return Err(RapidOcrError::Config(format!(
                    "formula detect model output channel count is dynamic and metadata `names` \
                     could not be parsed; cannot determine the class count (model={})",
                    model_path.display()
                )));
            }
            Ok(parsed)
        }
    }
}

/// 阈值校验：越界或 NaN 一律拒绝。
fn validate_detect_options(options: &FormulaDetectOptions) -> Result<()> {
    if !(0.0..=1.0).contains(&options.confidence_threshold) {
        return Err(RapidOcrError::InvalidInput(format!(
            "formula detect confidence_threshold must be within 0.0..=1.0, got {}",
            options.confidence_threshold
        )));
    }
    if !(0.0..=1.0).contains(&options.iou_threshold) {
        return Err(RapidOcrError::InvalidInput(format!(
            "formula detect iou_threshold must be within 0.0..=1.0, got {}",
            options.iou_threshold
        )));
    }
    Ok(())
}

/// 单张图片的 letterbox 几何，均相对固定的 768x768 输入。
#[derive(Debug, Clone, Copy, PartialEq)]
struct LetterboxMeta {
    /// 等比缩放系数 `r`。
    scale: f32,
    /// 左侧填充列数（Ultralytics `round(dw - 0.1)`）。
    left: f32,
    /// 顶部填充行数（Ultralytics `round(dh - 0.1)`）。
    top: f32,
    /// 缩放后的内容尺寸。
    new_size: (u32, u32),
}

/// letterbox 之后的网络输入。
#[derive(Debug)]
struct Letterboxed {
    /// `[1, 3, 768, 768]`，RGB、`/255`、NCHW。
    tensor: Array4<f32>,
    meta: LetterboxMeta,
}

/// 一个 batch 的 letterbox 结果。
#[derive(Debug)]
struct PreparedBatch {
    /// `[N, 3, 768, 768]`。
    tensor: Array4<f32>,
    metas: Vec<LetterboxMeta>,
    originals: Vec<(u32, u32)>,
}

/// 单张图片 letterbox 成 `[1, 3, 768, 768]`。
fn letterbox(image: &DynamicImage) -> Result<Letterboxed> {
    let plane = FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
    let mut buffer = vec![PAD_VALUE; 3 * plane];
    let meta = letterbox_into(image, &mut buffer)?;
    let shape = (1, 3, FORMULA_DETECT_INPUT_SIZE, FORMULA_DETECT_INPUT_SIZE);
    let tensor = Array4::from_shape_vec(shape, buffer)
        .map_err(|error| RapidOcrError::InvalidInput(format!("formula detect tensor: {error}")))?;
    Ok(Letterboxed { tensor, meta })
}

/// 批量 letterbox；空批次返回 `None`（无需推理）。
fn prepare_batch(images: &[DynamicImage]) -> Result<Option<PreparedBatch>> {
    if images.is_empty() {
        return Ok(None);
    }
    let plane = 3 * FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
    let mut buffer = Vec::with_capacity(images.len() * plane);
    let mut metas = Vec::with_capacity(images.len());
    let mut originals = Vec::with_capacity(images.len());
    for image in images {
        let start = buffer.len();
        buffer.resize(start + plane, PAD_VALUE);
        metas.push(letterbox_into(image, &mut buffer[start..])?);
        originals.push(image.dimensions());
    }
    let shape = (
        images.len(),
        3,
        FORMULA_DETECT_INPUT_SIZE,
        FORMULA_DETECT_INPUT_SIZE,
    );
    let tensor = Array4::from_shape_vec(shape, buffer)
        .map_err(|error| RapidOcrError::InvalidInput(format!("formula detect batch: {error}")))?;
    Ok(Some(PreparedBatch {
        tensor,
        metas,
        originals,
    }))
}

/// 把单张图片写进长度为 `3 * 768 * 768` 的 NCHW 缓冲区，返回 letterbox 几何。
///
/// 通道顺序固定为 RGB（见模块文档）：`DynamicImage` 已是 RGB，不做 BGR 交换。
fn letterbox_into(image: &DynamicImage, out: &mut [f32]) -> Result<LetterboxMeta> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(RapidOcrError::InvalidInput(format!(
            "formula detect requires a non-empty image, got {width}x{height}"
        )));
    }
    let plane = FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
    debug_assert_eq!(out.len(), 3 * plane);

    let size = FORMULA_DETECT_INPUT_SIZE as f32;
    let rgb = image.to_rgb8();
    let scale = (size / height as f32).min(size / width as f32);
    let new_w = ((width as f32) * scale).round().clamp(1.0, size) as u32;
    let new_h = ((height as f32) * scale).round().clamp(1.0, size) as u32;
    let resized = image::imageops::resize(&rgb, new_w, new_h, FilterType::Triangle);

    let dw = (size - new_w as f32) / 2.0;
    let dh = (size - new_h as f32) / 2.0;
    // Ultralytics `LetterBox`：left = round(dw - 0.1), top = round(dh - 0.1)。
    let left = (dw - 0.1).round();
    let top = (dh - 0.1).round();
    let meta = LetterboxMeta {
        scale,
        left,
        top,
        new_size: (resized.width(), resized.height()),
    };

    let (content_w, content_h) = meta.new_size;
    let limit = FORMULA_DETECT_INPUT_SIZE as i64;
    for y in 0..content_h {
        for x in 0..content_w {
            let dx = meta.left as i64 + i64::from(x);
            let dy = meta.top as i64 + i64::from(y);
            if !(0..limit).contains(&dx) || !(0..limit).contains(&dy) {
                continue;
            }
            let pixel = resized.get_pixel(x, y);
            let offset = dy as usize * FORMULA_DETECT_INPUT_SIZE + dx as usize;
            out[offset] = f32::from(pixel[0]) / 255.0;
            out[plane + offset] = f32::from(pixel[1]) / 255.0;
            out[2 * plane + offset] = f32::from(pixel[2]) / 255.0;
        }
    }
    Ok(meta)
}

/// 解码单张图片的输出切片（`[4 + nc, A]`），返回原图坐标下的检测框。
///
/// batch 维由调用方切片；本函数是纯函数，因此可以脱离 ONNX session 单测。
fn decode_output(
    output: ArrayView2<'_, f32>,
    lb: &LetterboxMeta,
    original: (u32, u32),
    options: &FormulaDetectOptions,
    class_names: &[String],
) -> Vec<FormulaBox> {
    let nc = class_names.len();
    let anchors = output.shape()[1];
    let mut detections = Vec::new();
    for anchor in 0..anchors {
        let mut best_class = 0;
        let mut best_score = f32::NEG_INFINITY;
        for class in 0..nc {
            let score = output[[4 + class, anchor]];
            if score > best_score {
                best_score = score;
                best_class = class;
            }
        }
        // 用 `partial_cmp` 显式表达“严格大于阈值”：NaN 分数在这里被丢弃。
        if best_score.partial_cmp(&options.confidence_threshold) != Some(Ordering::Greater) {
            continue;
        }
        // 导出的图已经输出绝对像素中心的 cx/cy，这里不再叠加任何 anchor 偏移。
        let cx = output[[0, anchor]];
        let cy = output[[1, anchor]];
        let w = output[[2, anchor]];
        let h = output[[3, anchor]];
        // 模型输出不受信任：坐标必须是有限值，且宽高必须为正。非有限值会让
        // `scale_back`/`clamp` 产生 NaN 框并一路传播成非法 `FormulaBox`。
        if !cx.is_finite() || !cy.is_finite() || !w.is_finite() || !h.is_finite() {
            continue;
        }
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        let xyxy = [cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0];
        if !xyxy.iter().all(|value| value.is_finite()) {
            continue;
        }
        let scaled = scale_back(xyxy, lb, original);
        // 缩放/裁剪后仍要求是有效正值矩形；退化框直接丢弃。
        if !scaled.iter().all(|value| value.is_finite())
            || scaled[2] <= scaled[0]
            || scaled[3] <= scaled[1]
        {
            continue;
        }
        detections.push(Detection {
            xyxy: scaled,
            score: best_score,
            class_id: best_class,
        });
    }
    nms(detections, options.iou_threshold, options.max_detections)
        .into_iter()
        .map(|detection| formula_box(detection, class_names))
        .collect()
}

/// 一个待 NMS 的候选框（已经是原图坐标）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Detection {
    xyxy: [f32; 4],
    score: f32,
    class_id: usize,
}

/// Ultralytics `scale_boxes`：减填充、除缩放，裁剪到原图并保证 `x1 <= x2`、`y1 <= y2`。
fn scale_back(xyxy: [f32; 4], lb: &LetterboxMeta, original: (u32, u32)) -> [f32; 4] {
    let width = original.0 as f32;
    let height = original.1 as f32;
    let x1 = ((xyxy[0] - lb.left) / lb.scale).clamp(0.0, width);
    let x2 = ((xyxy[2] - lb.left) / lb.scale).clamp(0.0, width);
    let y1 = ((xyxy[1] - lb.top) / lb.scale).clamp(0.0, height);
    let y2 = ((xyxy[3] - lb.top) / lb.scale).clamp(0.0, height);
    [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)]
}

/// 交并比；退化框（面积 0）返回 0。
fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let inter_w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let inter_h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let intersection = inter_w * inter_h;
    let area_a = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0);
    let area_b = (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0);
    let union = area_a + area_b - intersection;
    if union > 0.0 {
        intersection / union
    } else {
        0.0
    }
}

/// 类别感知贪心 NMS：按分数降序，IoU 严格大于阈值则抑制。
///
/// 与 Ultralytics 一致地把类别折算成坐标偏移（`c = class_id * 7680`），因此不同类别的框
/// 永远不会互相抑制；`max_detections == 0` 与 Ultralytics `max_det=0` 一样表示不限制。
fn nms(detections: Vec<Detection>, iou_threshold: f32, max_detections: usize) -> Vec<Detection> {
    let mut order: Vec<usize> = (0..detections.len()).collect();
    order.sort_by(|left, right| detections[*right].score.total_cmp(&detections[*left].score));
    let mut keep: Vec<Detection> = Vec::new();
    for index in order {
        if max_detections != 0 && keep.len() >= max_detections {
            break;
        }
        let candidate = detections[index];
        let candidate_box = offset_box(candidate);
        let suppressed = keep
            .iter()
            .any(|kept| iou(candidate_box, offset_box(*kept)) > iou_threshold);
        if !suppressed {
            keep.push(candidate);
        }
    }
    keep
}

/// 按类别把框搬到互不重叠的坐标区间（Ultralytics `boxes += c * max_wh`）。
fn offset_box(detection: Detection) -> [f32; 4] {
    let offset = detection.class_id as f32 * CLASS_OFFSET;
    [
        detection.xyxy[0] + offset,
        detection.xyxy[1] + offset,
        detection.xyxy[2] + offset,
        detection.xyxy[3] + offset,
    ]
}

/// `xyxy` -> 左上角起顺时针的四点多边形。
fn formula_box(detection: Detection, class_names: &[String]) -> FormulaBox {
    let [x1, y1, x2, y2] = detection.xyxy;
    FormulaBox {
        polygon: [[x1, y1], [x2, y1], [x2, y2], [x1, y2]],
        score: detection.score,
        class_id: detection.class_id,
        class_name: class_names
            .get(detection.class_id)
            .cloned()
            .unwrap_or_else(|| format!("class_{}", detection.class_id)),
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    /// 与 `tools/formula_detect_reference.py` 生成的 golden 对应。
    #[derive(Debug, Deserialize)]
    struct Golden {
        image: String,
        image_size: [u32; 2],
        pasted_rect: [f32; 4],
        confidence_threshold: f32,
        iou_threshold: f32,
        max_detections: usize,
        class_names: Vec<String>,
        anchor_count: usize,
        letterbox: GoldenLetterbox,
        raw_boxes: Vec<GoldenRawBox>,
        boxes: Vec<FormulaBox>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenLetterbox {
        scale: f32,
        left: f32,
        top: f32,
        new_size: [u32; 2],
    }

    #[derive(Debug, Deserialize)]
    struct GoldenRawBox {
        xyxy: [f32; 4],
        score: f32,
        class_id: usize,
    }

    /// 参考脚本产出的两套 fixture：单位缩放 + 顶部填充，以及 2/3 缩放 + 顶部填充。
    const GOLDENS: [&str; 2] = ["golden.json", "golden_scaled.json"];

    fn fixture_dir() -> PathBuf {
        crate::test_support::fixture_dir("formula-detect")
    }

    fn load_golden(name: &str) -> Golden {
        let path = fixture_dir().join(name);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    fn open_page(golden: &Golden) -> DynamicImage {
        let path = fixture_dir().join(&golden.image);
        image::open(&path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
    }

    /// 模型定位统一走 `crate::test_support`：环境变量解析、缺失资产处理与
    /// `RAPID_OCR_REQUIRE_EXTERNAL_ASSETS` 语义只有一份实现，避免测试之间语义分叉。
    fn detect_model_path() -> Option<PathBuf> {
        crate::test_support::formula_detector_path()
    }

    fn rgb_image(width: u32, height: u32, color: [u8; 3]) -> DynamicImage {
        DynamicImage::from(RgbImage::from_pixel(width, height, Rgb(color)))
    }

    fn xyxy_of(polygon: [[f32; 2]; 4]) -> [f32; 4] {
        [polygon[0][0], polygon[0][1], polygon[2][0], polygon[2][1]]
    }

    fn iou_with_rect(polygon: [[f32; 2]; 4], rect: [f32; 4]) -> f32 {
        iou(xyxy_of(polygon), rect)
    }

    fn approx(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-4
    }

    #[test]
    fn default_options_match_documented_constants() {
        let options = FormulaDetectOptions::default();
        assert!(approx(
            options.confidence_threshold,
            DEFAULT_FORMULA_DETECT_CONFIDENCE
        ));
        assert!(approx(options.iou_threshold, DEFAULT_FORMULA_DETECT_IOU));
        assert_eq!(
            options.max_detections,
            DEFAULT_FORMULA_DETECT_MAX_DETECTIONS
        );
    }

    /// 几何校验：Rust 的 letterbox 结论必须与 Python 参考逐字段一致。
    #[test]
    fn letterbox_geometry_matches_python_golden() {
        let plane = FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
        let expected_anchors: usize = [8_usize, 16, 32]
            .iter()
            .map(|stride| (FORMULA_DETECT_INPUT_SIZE / stride).pow(2))
            .sum();
        assert_eq!(expected_anchors, 12096);
        for name in GOLDENS {
            let golden = load_golden(name);
            assert_eq!(
                golden.anchor_count, expected_anchors,
                "{name}: stride 8/16/32 anchor count for a 768x768 input"
            );
            let page = open_page(&golden);
            assert_eq!(
                [page.width(), page.height()],
                golden.image_size,
                "{name}: fixture page size"
            );
            let letterboxed = letterbox(&page).expect("letterbox must succeed");
            assert_eq!(
                letterboxed.tensor.shape(),
                &[1, 3, FORMULA_DETECT_INPUT_SIZE, FORMULA_DETECT_INPUT_SIZE],
                "{name}: tensor shape"
            );
            assert!(
                approx(letterboxed.meta.scale, golden.letterbox.scale),
                "{name}: scale {} vs {}",
                letterboxed.meta.scale,
                golden.letterbox.scale
            );
            assert!(
                approx(letterboxed.meta.left, golden.letterbox.left),
                "{name}: left {} vs {}",
                letterboxed.meta.left,
                golden.letterbox.left
            );
            assert!(
                approx(letterboxed.meta.top, golden.letterbox.top),
                "{name}: top {} vs {}",
                letterboxed.meta.top,
                golden.letterbox.top
            );
            assert_eq!(
                letterboxed.meta.new_size,
                (golden.letterbox.new_size[0], golden.letterbox.new_size[1]),
                "{name}: resized content size"
            );

            // 填充区域必须是 114/255，且内容必须落在 (left, top) 处。
            let top = golden.letterbox.top as usize;
            if top > 0 {
                let padding = letterboxed.tensor.slice(s![0, 0, 0, ..]);
                assert!(
                    padding.iter().all(|value| approx(*value, PAD_VALUE)),
                    "{name}: row 0 must be padding"
                );
                let content = letterboxed.tensor.slice(s![0, 0, top, ..]);
                assert!(
                    content.iter().any(|value| !approx(*value, PAD_VALUE)),
                    "{name}: content must start at row {top}"
                );
            }
            let values: Vec<f32> = letterboxed.tensor.iter().copied().collect();
            assert!(
                values.iter().all(|value| (0.0..=1.0).contains(value)),
                "{name}: normalized range"
            );
            assert_eq!(3 * plane, letterboxed.tensor.len());
        }
    }

    /// golden 的 letterboxed 原始输出经 Rust 的 `scale_back` + `nms` 必须复现 Python
    /// 参考给出的最终多边形（1e-3 像素）。
    #[test]
    fn rust_scale_back_and_nms_reproduce_python_golden() {
        for name in GOLDENS {
            let golden = load_golden(name);
            let page = open_page(&golden);
            let letterboxed = letterbox(&page).expect("letterbox must succeed");
            let original = (page.width(), page.height());

            let detections: Vec<Detection> = golden
                .raw_boxes
                .iter()
                .map(|raw| Detection {
                    xyxy: scale_back(raw.xyxy, &letterboxed.meta, original),
                    score: raw.score,
                    class_id: raw.class_id,
                })
                .collect();
            let kept = nms(detections, golden.iou_threshold, golden.max_detections);
            let boxes: Vec<FormulaBox> = kept
                .iter()
                .map(|detection| formula_box(*detection, &golden.class_names))
                .collect();

            assert!(
                golden.raw_boxes.len() > golden.boxes.len(),
                "{name}: fixture must exercise NMS ({} raw -> {} kept)",
                golden.raw_boxes.len(),
                golden.boxes.len()
            );
            assert_eq!(
                boxes.len(),
                golden.boxes.len(),
                "{name}: box count after NMS"
            );

            let mut max_delta = 0.0_f32;
            for (actual, expected) in boxes.iter().zip(golden.boxes.iter()) {
                assert_eq!(actual.class_id, expected.class_id, "{name}: class id");
                assert_eq!(actual.class_name, expected.class_name, "{name}: class name");
                assert!(
                    (actual.score - expected.score).abs() < 1e-3,
                    "{name}: score {} vs {}",
                    actual.score,
                    expected.score
                );
                for (point, expected_point) in actual.polygon.iter().zip(expected.polygon.iter()) {
                    for axis in 0..2 {
                        let delta = (point[axis] - expected_point[axis]).abs();
                        max_delta = max_delta.max(delta);
                        assert!(
                            delta < 1e-3,
                            "{name}: polygon delta {delta} ({point:?} vs {expected_point:?})"
                        );
                    }
                }
                // 多边形必须保持左上/右下关系（原点在左上角）。
                assert!(actual.polygon[0][0] < actual.polygon[2][0], "{name}");
                assert!(actual.polygon[0][1] < actual.polygon[2][1], "{name}");
            }
            eprintln!("{name}: max polygon delta vs python reference = {max_delta:e}");
        }
    }

    /// 独立 ground truth：检测器必须找到我们贴上去的那张公式图。
    #[test]
    fn detected_box_overlaps_the_pasted_formula_rect() {
        for name in GOLDENS {
            let golden = load_golden(name);
            assert!(
                !golden.boxes.is_empty(),
                "{name}: reference produced no box; fixture is useless"
            );
            let best = golden
                .boxes
                .iter()
                .map(|detected| iou_with_rect(detected.polygon, golden.pasted_rect))
                .fold(0.0_f32, f32::max);
            assert!(
                best >= 0.5,
                "{name}: best IoU with pasted_rect {:?} is {best}; detector missed the fixture",
                golden.pasted_rect
            );
            eprintln!("{name}: best IoU with pasted_rect = {best:.4}");
        }
    }

    /// 空图必须结构化报错；空批次不触发推理；阈值越界同样拒绝。
    #[test]
    fn empty_images_and_invalid_thresholds_are_rejected() {
        for (width, height) in [(0, 0), (0, 32), (32, 0)] {
            let error = letterbox(&rgb_image(width, height, [255, 255, 255]))
                .expect_err("empty image must fail");
            assert!(
                matches!(error, RapidOcrError::InvalidInput(_)),
                "{width}x{height}: {error}"
            );
            assert!(error.to_string().contains("non-empty"), "{error}");
        }
        assert!(prepare_batch(&[]).expect("empty batch is fine").is_none());

        let cases = [
            FormulaDetectOptions {
                confidence_threshold: 1.5,
                ..FormulaDetectOptions::default()
            },
            FormulaDetectOptions {
                confidence_threshold: -0.1,
                ..FormulaDetectOptions::default()
            },
            FormulaDetectOptions {
                confidence_threshold: f32::NAN,
                ..FormulaDetectOptions::default()
            },
            FormulaDetectOptions {
                iou_threshold: 1.2,
                ..FormulaDetectOptions::default()
            },
            FormulaDetectOptions {
                iou_threshold: -0.5,
                ..FormulaDetectOptions::default()
            },
            FormulaDetectOptions {
                iou_threshold: f32::NAN,
                ..FormulaDetectOptions::default()
            },
        ];
        for options in cases {
            let error = validate_detect_options(&options).expect_err("must be rejected");
            assert!(
                matches!(error, RapidOcrError::InvalidInput(_)),
                "{options:?}: {error}"
            );
        }
        assert!(validate_detect_options(&FormulaDetectOptions::default()).is_ok());
        assert!(
            validate_detect_options(&FormulaDetectOptions {
                confidence_threshold: 0.0,
                iou_threshold: 1.0,
                max_detections: 0,
            })
            .is_ok()
        );
    }

    /// 通道顺序锁：纯红/纯蓝图在 RGB（而非 BGR）布局下通道 0/2 互换。
    #[test]
    fn letterbox_channel_order_is_rgb() {
        let plane = FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
        let red = letterbox(&rgb_image(64, 64, [255, 0, 0])).expect("red letterbox");
        let blue = letterbox(&rgb_image(64, 64, [0, 0, 255])).expect("blue letterbox");
        let red_data = red.tensor.as_slice().expect("contiguous");
        let blue_data = blue.tensor.as_slice().expect("contiguous");
        let channel = |data: &[f32], index: usize| -> f32 {
            data[index * plane..(index + 1) * plane].iter().sum::<f32>() / plane as f32
        };
        // RGB：红图通道 0 全亮、通道 2 全灭；蓝图相反。BGR 布局下结论必然相反。
        assert!(approx(channel(red_data, 0), 1.0), "red R");
        assert!(approx(channel(red_data, 1), 0.0), "red G");
        assert!(approx(channel(red_data, 2), 0.0), "red B");
        assert!(approx(channel(blue_data, 0), 0.0), "blue R");
        assert!(approx(channel(blue_data, 1), 0.0), "blue G");
        assert!(approx(channel(blue_data, 2), 1.0), "blue B");
        // 两张图必须产生不同的网络输入。
        assert_ne!(red_data[..plane], blue_data[..plane]);
        assert_ne!(red.tensor, blue.tensor);
    }

    /// 竖图（宽 < 高）触发左侧填充，同时锁定写入偏移（不只是 meta）。
    #[test]
    fn letterbox_pads_portrait_images_horizontally() {
        let plane = FORMULA_DETECT_INPUT_SIZE * FORMULA_DETECT_INPUT_SIZE;
        let portrait = rgb_image(704, 768, [255, 255, 255]);
        let letterboxed = letterbox(&portrait).expect("letterbox");
        assert!(approx(letterboxed.meta.scale, 1.0));
        assert_eq!(letterboxed.meta.new_size, (704, 768));
        assert!(approx(letterboxed.meta.left, 32.0));
        assert!(approx(letterboxed.meta.top, 0.0));

        let data = letterboxed.tensor.as_slice().expect("contiguous");
        let row = &data[..FORMULA_DETECT_INPUT_SIZE];
        assert!(
            row[..32].iter().all(|value| approx(*value, PAD_VALUE)),
            "left padding must be 114"
        );
        assert!(
            row[32..32 + 704].iter().all(|value| approx(*value, 1.0)),
            "content must start at column 32"
        );
        assert!(
            row[32 + 704..]
                .iter()
                .all(|value| approx(*value, PAD_VALUE)),
            "right padding must be 114"
        );
        assert_eq!(3 * plane, data.len());
    }

    /// `scale_back` 的减填充/除缩放/裁剪/归一化。
    #[test]
    fn scale_back_subtracts_padding_divides_and_clamps() {
        let meta = LetterboxMeta {
            scale: 0.5,
            left: 100.0,
            top: 20.0,
            new_size: (768, 384),
        };
        assert_eq!(
            scale_back([100.0, 20.0, 500.0, 220.0], &meta, (1920, 1080)),
            [0.0, 0.0, 800.0, 400.0]
        );
        // 超出原图的坐标裁剪到图像边界。
        assert_eq!(
            scale_back([0.0, 0.0, 5000.0, 5000.0], &meta, (100, 100)),
            [0.0, 0.0, 100.0, 100.0]
        );
        // 顺序颠倒时归一化成左上/右下。
        assert_eq!(
            scale_back([500.0, 220.0, 100.0, 20.0], &meta, (1920, 1080)),
            [0.0, 0.0, 800.0, 400.0]
        );
    }

    /// NMS：同类别抑制、跨类别保留、上限截断、按分数降序。
    #[test]
    fn nms_is_class_aware_sorted_and_capped() {
        let boxed = |x: f32, score: f32, class_id: usize| Detection {
            xyxy: [x, 0.0, x + 10.0, 10.0],
            score,
            class_id,
        };

        let kept = nms(vec![boxed(0.0, 0.9, 0), boxed(0.0, 0.8, 0)], 0.7, 300);
        assert_eq!(kept.len(), 1, "same class must be suppressed");
        assert!(approx(kept[0].score, 0.9));

        // 完全重叠但类别不同：Ultralytics 的 class offset 让两者都保留。
        let kept = nms(vec![boxed(0.0, 0.9, 0), boxed(0.0, 0.8, 1)], 0.7, 300);
        assert_eq!(kept.len(), 2, "class-aware NMS must keep both classes");

        let many: Vec<Detection> = (0..10)
            .map(|index| boxed(index as f32 * 100.0, 1.0 - index as f32 * 0.01, 0))
            .collect();
        assert_eq!(nms(many.clone(), 0.7, 3).len(), 3);
        // Ultralytics 语义：max_det=0 表示不限制。
        assert_eq!(nms(many, 0.7, 0).len(), 10);

        let unordered = vec![
            boxed(0.0, 0.3, 0),
            boxed(100.0, 0.95, 0),
            boxed(0.0, 0.5, 0),
            boxed(200.0, 0.4, 0),
        ];
        let scores: Vec<f32> = nms(unordered, 0.7, 300)
            .iter()
            .map(|detection| detection.score)
            .collect();
        assert_eq!(scores, vec![0.95, 0.5, 0.4]);
    }

    #[test]
    fn names_metadata_parser_handles_ultralytics_format() {
        assert_eq!(
            parse_names_metadata("{0: 'embedding', 1: 'isolated'}"),
            vec!["embedding".to_string(), "isolated".to_string()]
        );
        assert_eq!(
            parse_names_metadata(r#"{0: "a", 1: "b"}"#),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_names_metadata("{1: 'b', 0: 'a'}"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_names_metadata("not a dict").is_empty());
        assert!(parse_names_metadata("{0: 'a', 2: 'b'}").is_empty());
        assert!(parse_names_metadata("{0: 'a'}extra").is_empty());
        assert!(parse_names_metadata("{'a': 'b'}").is_empty());
        assert!(parse_names_metadata("{0: ''}").is_empty());
    }

    #[test]
    fn class_names_fall_back_to_index_when_metadata_is_unusable() {
        let model = Path::new("dummy.onnx");
        assert_eq!(
            resolve_class_names(Some("{0: 'a', 1: 'b'}"), Some(2), model).expect("parsed"),
            vec!["a".to_string(), "b".to_string()]
        );
        for raw in [None, Some("{0: 'a'}"), Some("garbage")] {
            assert_eq!(
                resolve_class_names(raw, Some(2), model).expect("fallback"),
                vec!["class_0".to_string(), "class_1".to_string()],
                "raw={raw:?}"
            );
        }
        assert_eq!(
            resolve_class_names(Some("{0: 'a', 1: 'b'}"), None, model).expect("dynamic channels"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(resolve_class_names(None, None, model).is_err());
    }

    #[test]
    fn contract_validation_accepts_the_real_signature_and_rejects_mismatches() {
        fn spec(name: &str, dims: Vec<i64>, element_type: TensorElementType) -> TensorSpec {
            TensorSpec {
                name: name.to_string(),
                rank: dims.len(),
                dims,
                element_type,
            }
        }
        fn probe(input_dims: Vec<i64>, output_dims: Vec<i64>) -> ModelIoProbe {
            ModelIoProbe {
                inputs: vec![spec("images", input_dims, TensorElementType::Float32)],
                outputs: vec![spec("output0", output_dims, TensorElementType::Float32)],
            }
        }
        let path = Path::new("mfd.onnx");
        assert_eq!(
            validate_detect_contract(&probe(vec![-1, 3, -1, -1], vec![-1, 6, -1]), path)
                .expect("real signature must pass"),
            DetectContract {
                num_classes: Some(2)
            }
        );
        assert!(
            validate_detect_contract(&probe(vec![1, 3, 768, 768], vec![1, 6, 12096]), path).is_ok()
        );
        assert_eq!(
            validate_detect_contract(&probe(vec![-1, 3, -1, -1], vec![-1, -1, -1]), path)
                .expect("dynamic channels"),
            DetectContract { num_classes: None }
        );

        let wrong_channels = probe(vec![-1, 1, -1, -1], vec![-1, 6, -1]);
        assert!(validate_detect_contract(&wrong_channels, path).is_err());

        let wrong_input_size = probe(vec![-1, 3, 640, 640], vec![-1, 6, -1]);
        let error = validate_detect_contract(&wrong_input_size, path).expect_err("640 must fail");
        assert!(error.to_string().contains("768"), "{error}");

        let no_class_channels = probe(vec![-1, 3, -1, -1], vec![-1, 4, -1]);
        let error =
            validate_detect_contract(&no_class_channels, path).expect_err("4 channels must fail");
        assert!(error.to_string().contains("class score"), "{error}");

        let wrong_output_rank = probe(vec![-1, 3, -1, -1], vec![-1, 6]);
        let error =
            validate_detect_contract(&wrong_output_rank, path).expect_err("rank2 must fail");
        assert!(error.to_string().contains("rank"), "{error}");

        let wrong_dtype = ModelIoProbe {
            inputs: vec![spec(
                "images",
                vec![-1, 3, -1, -1],
                TensorElementType::Int64,
            )],
            outputs: vec![spec("output0", vec![-1, 6, -1], TensorElementType::Float32)],
        };
        let error = validate_detect_contract(&wrong_dtype, path).expect_err("int64 must fail");
        assert!(error.to_string().contains("FLOAT32"), "{error}");

        let multi_output = ModelIoProbe {
            inputs: vec![spec(
                "images",
                vec![-1, 3, -1, -1],
                TensorElementType::Float32,
            )],
            outputs: vec![
                spec("output0", vec![-1, 6, -1], TensorElementType::Float32),
                spec("output1", vec![-1, 6, -1], TensorElementType::Float32),
            ],
        };
        let error = validate_detect_contract(&multi_output, path).expect_err("multi must fail");
        assert!(error.to_string().contains("exactly one output"), "{error}");
    }

    #[test]
    fn missing_model_file_is_reported() {
        let path = fixture_dir().join("no_such_model.onnx");
        let error = FormulaDetector::from_model(&path, &RuntimeConfig::default())
            .expect_err("missing model must fail");
        assert!(matches!(error, RapidOcrError::FileNotFound(_)), "{error}");
    }

    /// 纯函数 decode：手工构造 `[6, A]` 输出，锁定“不再叠加 anchor 偏移”的语义。
    #[test]
    fn decode_output_uses_absolute_centres_without_anchor_offsets() {
        let class_names = ["embedding".to_string(), "isolated".to_string()];
        // 单 anchor：cx/cy/w/h = 100/200/40/20（已经是输入像素），置信度 0.9 属于类 1。
        let mut output = ndarray::Array2::<f32>::zeros((6, 1));
        output[[0, 0]] = 100.0;
        output[[1, 0]] = 200.0;
        output[[2, 0]] = 40.0;
        output[[3, 0]] = 20.0;
        output[[4, 0]] = 0.1;
        output[[5, 0]] = 0.9;
        let meta = LetterboxMeta {
            scale: 1.0,
            left: 0.0,
            top: 192.0,
            new_size: (768, 384),
        };
        let options = FormulaDetectOptions::default();
        let boxes = decode_output(output.view(), &meta, (768, 384), &options, &class_names);
        assert_eq!(boxes.len(), 1);
        // 中心 (100, 200) 尺寸 40x20 -> xyxy (80, 190, 120, 210)，再减 top=192 得到
        // (80, -2, 120, 18)，裁剪后是 (80, 0, 120, 18)。若错误地再叠加 stride/anchor
        // 偏移，坐标就不再是这个值。
        let expected = [[80.0, 0.0], [120.0, 0.0], [120.0, 18.0], [80.0, 18.0]];
        for (point, expected_point) in boxes[0].polygon.iter().zip(expected.iter()) {
            assert!(approx(point[0], expected_point[0]), "{point:?}");
            assert!(approx(point[1], expected_point[1]), "{point:?}");
        }
        assert_eq!(boxes[0].class_id, 1);
        assert_eq!(boxes[0].class_name, "isolated");
        assert!(approx(boxes[0].score, 0.9));

        // 阈值是严格大于；等于阈值以及 NaN 分数都必须被丢弃。
        output[[5, 0]] = DEFAULT_FORMULA_DETECT_CONFIDENCE;
        assert!(
            decode_output(output.view(), &meta, (768, 384), &options, &class_names).is_empty(),
            "score == threshold must be dropped"
        );
        output[[4, 0]] = f32::NAN;
        output[[5, 0]] = f32::NAN;
        assert!(
            decode_output(output.view(), &meta, (768, 384), &options, &class_names).is_empty(),
            "NaN scores must be dropped"
        );
    }

    /// 模型输出不受信任：非有限坐标与退化宽高必须在解码阶段被丢弃，
    /// 不能生成非法 `FormulaBox`。
    #[test]
    fn decode_output_drops_non_finite_and_degenerate_boxes() {
        let class_names = ["embedding".to_string(), "isolated".to_string()];
        let meta = LetterboxMeta {
            scale: 1.0,
            left: 0.0,
            top: 0.0,
            new_size: (768, 768),
        };
        let options = FormulaDetectOptions::default();

        // 每条 case 只放一个高置信度 anchor，其它 anchor 全部低于阈值。
        let cases: [(&str, [f32; 4]); 9] = [
            ("finite control", [100.0, 100.0, 40.0, 40.0]),
            ("NaN cx", [f32::NAN, 100.0, 40.0, 40.0]),
            ("NaN cy", [100.0, f32::NAN, 40.0, 40.0]),
            ("infinite w", [100.0, 100.0, f32::INFINITY, 40.0]),
            ("negative w", [100.0, 100.0, -40.0, 40.0]),
            ("zero h", [100.0, 100.0, 40.0, 0.0]),
            ("negative h", [100.0, 100.0, 40.0, -1.0]),
            ("NaN w", [100.0, 100.0, f32::NAN, 40.0]),
            (
                "negative infinity h",
                [100.0, 100.0, 40.0, f32::NEG_INFINITY],
            ),
        ];

        for (name, values) in cases {
            let mut output = ndarray::Array2::<f32>::zeros((6, 1));
            output[[0, 0]] = values[0];
            output[[1, 0]] = values[1];
            output[[2, 0]] = values[2];
            output[[3, 0]] = values[3];
            output[[4, 0]] = 0.05;
            output[[5, 0]] = 0.9;
            let boxes = decode_output(output.view(), &meta, (768, 768), &options, &class_names);
            if name == "finite control" {
                assert_eq!(boxes.len(), 1, "{name} must be kept");
                continue;
            }
            assert!(boxes.is_empty(), "{name} must be dropped, got {boxes:?}");
        }
    }

    /// 退化框（缩放到原图后宽或高为 0）也必须被丢弃。
    #[test]
    fn decode_output_drops_boxes_that_collapse_after_scale_back() {
        let class_names = ["embedding".to_string(), "isolated".to_string()];
        let meta = LetterboxMeta {
            scale: 2.0,
            left: 0.0,
            top: 0.0,
            new_size: (768, 768),
        };
        let options = FormulaDetectOptions::default();
        let mut output = ndarray::Array2::<f32>::zeros((6, 1));
        // 0.1 像素宽 -> 除以 scale 后仍然大于 0，但中心落在左边界外会被裁剪成 0 宽。
        output[[0, 0]] = -500.0;
        output[[1, 0]] = 100.0;
        output[[2, 0]] = 40.0;
        output[[3, 0]] = 40.0;
        output[[5, 0]] = 0.9;
        assert!(
            decode_output(output.view(), &meta, (768, 768), &options, &class_names).is_empty(),
            "a box clipped to zero width must be dropped"
        );
    }

    /// 模型可用时做端到端校验；缺失则跳过（不 panic、不写死绝对路径）。
    #[test]
    fn model_backed_detection_matches_reference_when_available() {
        let Some(model_path) = detect_model_path() else {
            return;
        };
        let runtime = RuntimeConfig::default();
        let mut detector = FormulaDetector::from_model(&model_path, &runtime)
            .expect("pix2text-mfd-1.5 must satisfy the detect contract");
        let golden = load_golden("golden.json");
        assert_eq!(
            detector.class_names(),
            golden.class_names.as_slice(),
            "class names must come from the `names` metadata"
        );
        assert!(matches!(
            detector.provider_resolution().selected_ep,
            crate::runtime::provider::ResolvedExecutionProvider::Cpu
        ));

        let options = FormulaDetectOptions {
            confidence_threshold: golden.confidence_threshold,
            iou_threshold: golden.iou_threshold,
            max_detections: golden.max_detections,
        };
        let page = open_page(&golden);
        let boxes = detector.detect(&page, &options).expect("detect must run");
        assert!(
            !boxes.is_empty(),
            "model found no formula on the fixture page"
        );
        for detected in &boxes {
            eprintln!(
                "detected {} score={:.6} polygon={:?}",
                detected.class_name, detected.score, detected.polygon
            );
        }
        let best = boxes
            .iter()
            .map(|detected| iou_with_rect(detected.polygon, golden.pasted_rect))
            .fold(0.0_f32, f32::max);
        assert!(
            best >= 0.5,
            "Rust detection missed the pasted formula rect: best IoU = {best}"
        );

        // 与独立 Python 参考对比：同一张图、同一阈值，box 必须几乎重合。
        let reference = &golden.boxes[0];
        let reference_xyxy = xyxy_of(reference.polygon);
        let (closest, closest_iou) = boxes
            .iter()
            .map(|detected| {
                (
                    xyxy_of(detected.polygon),
                    iou(xyxy_of(detected.polygon), reference_xyxy),
                )
            })
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .expect("at least one detection");
        let delta = closest
            .iter()
            .zip(reference_xyxy.iter())
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f32, f32::max);
        eprintln!(
            "rust vs python reference: IoU={closest_iou:.4} max-coord-delta={delta:.4}px, \
             best-IoU-with-pasted-rect={best:.4}"
        );
        assert!(
            closest_iou >= 0.97,
            "Rust and Python reference disagree: IoU = {closest_iou}"
        );
        assert!(
            delta <= 5.0,
            "Rust and Python reference disagree: max coord delta = {delta}px"
        );

        // 缩放路径（1152x576 页面 -> letterbox scale 2/3）同样必须复现参考解。
        let scaled = load_golden("golden_scaled.json");
        let scaled_page = open_page(&scaled);
        let scaled_boxes = detector
            .detect(&scaled_page, &options)
            .expect("scaled detect must run");
        assert!(!scaled_boxes.is_empty(), "scaled page produced no box");
        let scaled_best = scaled_boxes
            .iter()
            .map(|detected| iou_with_rect(detected.polygon, scaled.pasted_rect))
            .fold(0.0_f32, f32::max);
        let scaled_reference_xyxy = xyxy_of(scaled.boxes[0].polygon);
        let scaled_actual_xyxy = xyxy_of(scaled_boxes[0].polygon);
        let scaled_delta = scaled_actual_xyxy
            .iter()
            .zip(scaled_reference_xyxy.iter())
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f32, f32::max);
        eprintln!(
            "scaled page: best-IoU-with-pasted-rect={scaled_best:.4}, \
             max-coord-delta-vs-reference={scaled_delta:.4}px"
        );
        assert!(
            scaled_best >= 0.5,
            "scaled page must find the pasted formula: IoU = {scaled_best}"
        );
        assert!(
            scaled_delta <= 5.0,
            "scaled page: max coord delta vs reference = {scaled_delta}px"
        );

        // 批量：顺序必须与输入一致，空批次必须为空。
        let blank = rgb_image(64, 64, [255, 255, 255]);
        let batch = detector
            .detect_batch(&[page.clone(), blank.clone()], &options)
            .expect("batch detect must run");
        assert_eq!(batch.len(), 2, "results[i] must belong to images[i]");
        assert_eq!(batch[1].len(), 0, "blank page must yield no detection");
        assert_eq!(batch[0].len(), boxes.len());
        let batch_iou = batch[0]
            .first()
            .map(|detected| iou_with_rect(detected.polygon, xyxy_of(boxes[0].polygon)))
            .unwrap_or(0.0);
        assert!(
            batch_iou >= 0.99,
            "single and batch paths must agree, IoU = {batch_iou}"
        );
        assert!(
            detector
                .detect_batch(&[], &options)
                .expect("empty batch")
                .is_empty()
        );

        // 空图与非法阈值必须结构化报错（而不是 panic）。
        let error = detector
            .detect(&DynamicImage::new_rgb8(0, 0), &options)
            .expect_err("0x0 image must fail");
        assert!(matches!(error, RapidOcrError::InvalidInput(_)), "{error}");
        let error = detector
            .detect(
                &page,
                &FormulaDetectOptions {
                    confidence_threshold: 2.0,
                    ..options
                },
            )
            .expect_err("threshold 2.0 must fail");
        assert!(matches!(error, RapidOcrError::InvalidInput(_)), "{error}");
    }
}

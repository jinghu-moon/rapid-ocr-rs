use std::time::Instant;
use std::{
    sync::{Arc, Once},
    thread,
};

use crate::{
    api::OcrEngine as _,
    error::Result,
    input::image_loader::{LoadImage, OcrInput, ensure_decode_pixels},
    ocr::cls::classifier::{Classifier, ClassifierConfig},
    ocr::config::RecognizeOptions,
    ocr::det::detector::{Detector, DetectorConfig},
    ocr::pipeline::{
        config::EngineConfig,
        image_ops::{
            PreprocessRecord, apply_vertical_padding, crop_text_regions, map_boxes_to_original,
            map_img_to_original, resize_image_within_bounds,
        },
        types::{ExecutionOptions, ExecutionOutput},
    },
    ocr::rec::recognizer::Recognizer,
    ocr::types::{LineResult, WordBox},
    runtime::provider::ProviderResolution,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineProviderResolutions {
    pub det: ProviderResolution,
    pub cls: Option<ProviderResolution>,
    pub rec: ProviderResolution,
}

#[derive(Debug, Clone, Copy)]
struct RunSwitches {
    use_det: bool,
    use_cls: bool,
    apply_cls_rotation: bool,
    use_rec: bool,
    need_stage_images: bool,
    return_word_box: bool,
    return_single_char_box: bool,
    text_score: f32,
}

#[derive(Debug)]
struct PreparedImage {
    ori_h: usize,
    ori_w: usize,
    ratio_h: f32,
    ratio_w: f32,
    preprocess_record: PreprocessRecord,
    proc_img: crate::config::RecImage,
    decode_ms: f32,
    resize_ms: f32,
}

#[derive(Debug, Default)]
struct RunBuffers {
    det_boxes: Vec<crate::Quad>,
    det_scores: Vec<f32>,
    stage_images: Vec<crate::config::RecImage>,
    lines: Vec<LineResult>,
}

#[derive(Debug)]
pub struct RapidOcr {
    config: EngineConfig,
    detector: Detector,
    classifier: Option<Classifier>,
    recognizer: Recognizer,
    loader: LoadImage,
}

impl RapidOcr {
    pub fn new(config: EngineConfig) -> Result<Self> {
        init_rayon_global_pool(&config);
        let det = Detector::new(detector_cfg_from_pipeline(&config))?;
        let cls = if config.global.use_cls {
            Some(Classifier::new(classifier_cfg_from_pipeline(&config))?)
        } else {
            None
        };
        let rec = Recognizer::new(config.rec.clone())?;
        Ok(Self {
            config,
            detector: det,
            classifier: cls,
            recognizer: rec,
            loader: LoadImage::default(),
        })
    }

    pub(crate) fn run(
        &mut self,
        input: OcrInput,
        opts: ExecutionOptions,
    ) -> Result<ExecutionOutput> {
        let e2e_start = Instant::now();
        let mut output = ExecutionOutput::default();
        let switches = self.resolve_run_switches(&opts);
        let mut prepared = self.prepare_image(input, switches.use_det)?;
        output.decode_ms = Some(prepared.decode_ms);
        output.resize_ms = Some(prepared.resize_ms);
        output.processed_size = Some((
            prepared.proc_img.width() as u32,
            prepared.proc_img.height() as u32,
        ));
        let mut buffers = RunBuffers::default();

        if !self.run_detection_stage(&opts, switches, &mut prepared, &mut buffers, &mut output)? {
            output.e2e_ms = Some(e2e_start.elapsed().as_secs_f32() * 1000.0);
            return Ok(output);
        }

        self.run_classification_stage(switches, &mut buffers, &mut output)?;
        self.run_recognition_stage(switches, &mut buffers, &mut output)?;
        let postprocess_start = Instant::now();
        self.finalize_detection_outputs(switches, &prepared, &mut buffers, &mut output)?;
        self.finalize_recognition_outputs(switches, &buffers.lines, &mut output);
        output.postprocess_ms = Some(postprocess_start.elapsed().as_secs_f32() * 1000.0);

        output.e2e_ms = Some(e2e_start.elapsed().as_secs_f32() * 1000.0);
        Ok(output)
    }

    fn resolve_run_switches(&self, opts: &ExecutionOptions) -> RunSwitches {
        let use_det = opts.use_det;
        let use_cls = opts.use_cls;
        let use_rec = opts.use_rec;
        RunSwitches {
            use_det,
            use_cls,
            apply_cls_rotation: opts.apply_cls_rotation,
            use_rec,
            need_stage_images: use_cls || use_rec,
            return_word_box: opts.return_word_box,
            return_single_char_box: opts.return_single_char_box,
            text_score: opts.text_score.unwrap_or(self.config.global.text_score),
        }
    }

    fn prepare_image(&mut self, input: OcrInput, use_det: bool) -> Result<PreparedImage> {
        let decode_start = Instant::now();
        let ori_img = self.loader.load(input)?;
        let decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
        let ori_h = ori_img.height();
        let ori_w = ori_img.width();
        let preprocessing_backend = if use_det {
            self.config.det.runtime.vision_backend
        } else {
            self.config.rec.runtime.vision_backend
        };
        let resize_start = Instant::now();
        let (proc_img, ratio_h, ratio_w) = resize_image_within_bounds(
            ori_img,
            self.config.global.min_side_len,
            self.config.global.max_side_len,
            preprocessing_backend,
        )?;
        let resize_ms = resize_start.elapsed().as_secs_f32() * 1000.0;

        Ok(PreparedImage {
            ori_h,
            ori_w,
            ratio_h,
            ratio_w,
            preprocess_record: PreprocessRecord {
                ratio_h,
                ratio_w,
                ..PreprocessRecord::default()
            },
            proc_img,
            decode_ms,
            resize_ms,
        })
    }

    fn run_detection_stage(
        &mut self,
        opts: &ExecutionOptions,
        switches: RunSwitches,
        prepared: &mut PreparedImage,
        buffers: &mut RunBuffers,
        output: &mut ExecutionOutput,
    ) -> Result<bool> {
        if switches.use_det {
            let (padded, pad_top) = apply_vertical_padding(
                prepared.proc_img.clone(),
                self.config.global.width_height_ratio,
                self.config.global.min_height,
            )?;
            prepared.proc_img = padded;
            prepared.preprocess_record.pad_top = pad_top;

            self.detector
                .update_postprocess(opts.box_thresh, opts.unclip_ratio);
            let det_out = self.detector.detect(&prepared.proc_img)?;
            if det_out.boxes.is_empty() {
                return Ok(false);
            }

            output.elapsed_ms[0] = Some(det_out.elapsed_ms);
            output.det_breakdown_ms = det_out.breakdown;
            buffers.det_boxes = det_out.boxes;
            buffers.det_scores = det_out.scores;

            if switches.need_stage_images {
                let crop_start = Instant::now();
                buffers.stage_images = crop_text_regions(
                    &prepared.proc_img,
                    &buffers.det_boxes,
                    self.config.det.runtime.vision_backend,
                )?;
                output.crop_ms = Some(crop_start.elapsed().as_secs_f32() * 1000.0);
            }
        } else if switches.need_stage_images {
            buffers.stage_images.push(prepared.proc_img.clone());
        }

        Ok(true)
    }

    fn run_classification_stage(
        &mut self,
        switches: RunSwitches,
        buffers: &mut RunBuffers,
        output: &mut ExecutionOutput,
    ) -> Result<()> {
        if switches.use_cls {
            let classifier = self.classifier.as_mut().ok_or_else(|| {
                crate::error::RapidOcrError::Config(
                    "classification requested but no classifier model is configured".to_string(),
                )
            })?;
            let cls_result = classifier
                .classify_in_place(&mut buffers.stage_images, switches.apply_cls_rotation)?;
            output.cls_res = Some(cls_result.cls_res);
            output.elapsed_ms[1] = Some(cls_result.elapsed_ms);
            output.cls_breakdown_ms = Some([
                cls_result.preprocess_ms,
                cls_result.infer_ms,
                cls_result.postprocess_ms,
            ]);
        }
        Ok(())
    }

    fn run_recognition_stage(
        &mut self,
        switches: RunSwitches,
        buffers: &mut RunBuffers,
        output: &mut ExecutionOutput,
    ) -> Result<()> {
        if !switches.use_rec {
            return Ok(());
        }

        let rec = self.recognizer.recognize(
            &buffers.stage_images,
            RecognizeOptions {
                return_word_box: switches.return_word_box,
                return_single_char_box: switches.return_single_char_box,
            },
        )?;
        output.elapsed_ms[2] = Some(rec.elapsed.as_secs_f32() * 1000.0);
        output.rec_breakdown_ms = Some([rec.preprocess_ms, rec.infer_ms, rec.postprocess_ms]);
        buffers.lines = rec.lines;
        Ok(())
    }

    fn finalize_detection_outputs(
        &mut self,
        switches: RunSwitches,
        prepared: &PreparedImage,
        buffers: &mut RunBuffers,
        output: &mut ExecutionOutput,
    ) -> Result<()> {
        if !switches.use_det {
            return Ok(());
        }

        let mut mapped_boxes = std::mem::take(&mut buffers.det_boxes);
        map_boxes_to_original(
            &mut mapped_boxes,
            prepared.preprocess_record,
            prepared.ori_h,
            prepared.ori_w,
        );

        if !switches.use_rec {
            output.boxes = Some(mapped_boxes);
            output.det_scores = Some(std::mem::take(&mut buffers.det_scores));
            return Ok(());
        }

        let lines = std::mem::take(&mut buffers.lines);
        let det_scores = std::mem::take(&mut buffers.det_scores);
        let (filtered_boxes, filtered_scores, filtered_lines, kept_indices) =
            filter_empty_lines_boxes_and_scores(mapped_boxes, det_scores, lines);
        buffers.lines = filtered_lines;

        let mut computed_word_boxes = None;
        if switches.return_word_box && !filtered_boxes.is_empty() && !buffers.lines.is_empty() {
            let mapped_crops = map_img_to_original(
                &buffers.stage_images,
                prepared.ratio_h,
                prepared.ratio_w,
                self.config.det.runtime.vision_backend,
            )?;
            let filtered_crops = select_items_by_indices(mapped_crops, &kept_indices);
            let word_boxes = crate::ocr::rec::word_boxes::compute_word_boxes_with_backend(
                &filtered_crops,
                &filtered_boxes,
                &buffers.lines,
                switches.return_single_char_box,
                self.config.rec.runtime.vision_backend,
            )?;
            computed_word_boxes = Some(word_boxes);
        }

        let (
            score_filtered_boxes,
            score_filtered_scores,
            score_filtered_lines,
            score_filtered_words,
        ) = filter_by_text_score_for_full(
            filtered_boxes,
            filtered_scores,
            std::mem::take(&mut buffers.lines),
            computed_word_boxes,
            switches.text_score,
        );

        buffers.lines = score_filtered_lines;
        output.boxes = Some(score_filtered_boxes);
        output.det_scores = Some(score_filtered_scores);
        output.word_boxes = score_filtered_words;
        Ok(())
    }

    fn finalize_recognition_outputs(
        &self,
        switches: RunSwitches,
        lines: &[LineResult],
        output: &mut ExecutionOutput,
    ) {
        if !switches.use_rec || lines.is_empty() {
            return;
        }

        // Keep parity with Python: text_score only filters full outputs (det+rec),
        // rec-only mode returns raw recognition results.
        output.txts = Some(lines.iter().map(|v| v.text.clone()).collect());
        output.scores = Some(lines.iter().map(|v| v.score).collect());
        output.lines = Some(lines.to_vec());
    }

    pub fn provider_resolutions(&self) -> PipelineProviderResolutions {
        PipelineProviderResolutions {
            det: self.detector.provider_resolution(),
            cls: self
                .classifier
                .as_ref()
                .map(Classifier::provider_resolution),
            rec: self.recognizer.provider_resolution(),
        }
    }

    pub fn set_max_side_len(&mut self, max_side_len: u32) {
        self.config.global.max_side_len = max_side_len.max(32) as usize;
    }

    pub fn set_image_bounds(&mut self, min_side_len: u32, max_side_len: u32) {
        self.config.global.min_side_len = min_side_len.max(1) as usize;
        self.config.global.max_side_len = max_side_len.max(32) as usize;
    }
}

fn init_rayon_global_pool(config: &EngineConfig) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let mut builder = rayon::ThreadPoolBuilder::new();
        if let Some(threads) = resolve_rayon_threads(config) {
            builder = builder.num_threads(threads.max(1));
        }
        let _ = builder.build_global();
    });
}

fn resolve_rayon_threads(config: &EngineConfig) -> Option<usize> {
    let runtimes = [
        &config.det.runtime,
        &config.cls.runtime,
        &config.rec.runtime,
    ];
    let explicit = runtimes
        .iter()
        .filter_map(|rt| rt.rayon_threads.filter(|v| *v > 0))
        .max();
    if explicit.is_some() {
        return explicit;
    }
    if runtimes.iter().any(|rt| rt.auto_tune_threads) {
        return available_parallelism();
    }
    None
}

fn available_parallelism() -> Option<usize> {
    let physical_cores = num_cpus::get_physical().max(1);
    thread::available_parallelism()
        .ok()
        .map(|v| v.get().clamp(1, physical_cores))
}

type FullFilterOutput = (
    Vec<crate::Quad>,
    Vec<f32>,
    Vec<LineResult>,
    Option<Vec<Vec<WordBox>>>,
);

fn filter_empty_lines_boxes_and_scores(
    boxes: Vec<crate::Quad>,
    det_scores: Vec<f32>,
    lines: Vec<LineResult>,
) -> (Vec<crate::Quad>, Vec<f32>, Vec<LineResult>, Vec<usize>) {
    if boxes.len() != lines.len() || det_scores.len() != lines.len() {
        return (boxes, det_scores, lines, Vec::new());
    }
    let mut out_boxes = Vec::new();
    let mut out_scores = Vec::new();
    let mut out_lines = Vec::new();
    let mut kept_indices = Vec::new();
    for (idx, ((b, s), l)) in boxes.into_iter().zip(det_scores).zip(lines).enumerate() {
        if l.text.trim().is_empty() {
            continue;
        }
        out_boxes.push(b);
        out_scores.push(s);
        out_lines.push(l);
        kept_indices.push(idx);
    }
    (out_boxes, out_scores, out_lines, kept_indices)
}

fn select_items_by_indices<T>(items: Vec<T>, indices: &[usize]) -> Vec<T> {
    if indices.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(indices.len());
    let mut keep_iter = indices.iter().copied().peekable();
    for (idx, item) in items.into_iter().enumerate() {
        let Some(target_idx) = keep_iter.peek().copied() else {
            break;
        };
        if idx == target_idx {
            out.push(item);
            keep_iter.next();
        }
    }
    out
}

fn filter_by_text_score_for_full(
    boxes: Vec<crate::Quad>,
    det_scores: Vec<f32>,
    lines: Vec<LineResult>,
    word_boxes: Option<Vec<Vec<WordBox>>>,
    text_score: f32,
) -> FullFilterOutput {
    let mut out_boxes = Vec::new();
    let mut out_scores = Vec::new();
    let mut out_lines = Vec::new();
    let mut out_word_boxes = Vec::new();

    for (idx, line) in lines.into_iter().enumerate() {
        if idx >= boxes.len() || idx >= det_scores.len() {
            break;
        }
        if line.score < text_score {
            continue;
        }
        out_boxes.push(boxes[idx]);
        out_scores.push(det_scores[idx]);
        if let Some(word_line) = word_boxes.as_ref().and_then(|v| v.get(idx)) {
            out_word_boxes.push(word_line.clone());
        }
        out_lines.push(line);
    }

    let out_word_boxes = if word_boxes.is_some() {
        Some(out_word_boxes)
    } else {
        None
    };

    (out_boxes, out_scores, out_lines, out_word_boxes)
}

fn detector_cfg_from_pipeline(config: &EngineConfig) -> DetectorConfig {
    config.det.clone()
}

fn rec_image_input(
    image: crate::config::RecImage,
    roi: Option<crate::api::RectU32>,
    max_decode_pixels: u64,
) -> Result<(OcrInput, crate::api::ImageSize, (u32, u32))> {
    let original_size = crate::api::ImageSize {
        width: image.width() as u32,
        height: image.height() as u32,
    };
    ensure_decode_pixels(
        original_size.width as usize,
        original_size.height as usize,
        max_decode_pixels,
    )?;
    let Some(roi) = roi else {
        return Ok((OcrInput::Image(image), original_size, (0, 0)));
    };
    roi.validate_against(original_size.width, original_size.height)?;
    let src = image.as_bgr_bytes();
    let row_bytes = roi.width as usize * 3;
    let mut out = vec![0_u8; row_bytes * roi.height as usize];
    for y in 0..roi.height as usize {
        let src_start = ((roi.y as usize + y) * image.width() + roi.x as usize) * 3;
        let dst_start = y * row_bytes;
        out[dst_start..dst_start + row_bytes]
            .copy_from_slice(&src[src_start..src_start + row_bytes]);
    }
    let cropped =
        crate::config::RecImage::from_bgr_u8(roi.width as usize, roi.height as usize, out)?;
    Ok((OcrInput::Image(cropped), original_size, (roi.x, roi.y)))
}

fn classifier_cfg_from_pipeline(config: &EngineConfig) -> ClassifierConfig {
    config.cls.clone()
}

fn rec_image_to_rgba(image: &crate::config::RecImage) -> Result<image::RgbaImage> {
    let bgr = image.as_bgr_cow();
    let mut rgba = vec![0_u8; image.width() * image.height() * 4];
    for (src, dst) in bgr
        .as_chunks::<3>()
        .0
        .iter()
        .zip(rgba.as_chunks_mut::<4>().0.iter_mut())
    {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, rgba)
        .ok_or_else(|| crate::error::RapidOcrError::InvalidImage("invalid image buffer".into()))
}

#[derive(Debug)]
pub struct RapidOcrEngine {
    inner: RapidOcr,
    model_id: String,
    base_min_side_len: usize,
    base_max_side_len: usize,
    /// 懒加载的公式识别器（模型路径 -> 实例），避免每次请求重建 594 MB session。
    formula_recognizer: Option<(
        std::path::PathBuf,
        crate::formula::recognizer::FormulaRecognizer,
    )>,
    /// 懒加载的页面公式检测器。
    formula_detector: Option<(std::path::PathBuf, crate::formula::detect::FormulaDetector)>,
}

impl RapidOcrEngine {
    pub fn new(config: EngineConfig) -> Result<Self> {
        let model_id = format!(
            "{}-{}-{}",
            config.rec.model.ocr_version.as_str(),
            config.rec.model.model_type.as_str(),
            config.rec.model.lang.as_str()
        );
        let base_min_side_len = config.global.min_side_len;
        let base_max_side_len = config.global.max_side_len;
        Ok(Self {
            inner: RapidOcr::new(config)?,
            model_id,
            base_min_side_len,
            base_max_side_len,
            formula_recognizer: None,
            formula_detector: None,
        })
    }

    pub fn provider_resolutions(&self) -> PipelineProviderResolutions {
        self.inner.provider_resolutions()
    }

    fn provider_info(&self) -> crate::api::ProviderInfo {
        let resolutions = self.inner.provider_resolutions();
        let convert = |resolution: ProviderResolution| crate::api::ProviderResolutionInfo {
            requested: match resolution.requested {
                crate::config::ProviderPreference::Cpu => crate::api::ProviderPreference::Cpu,
                crate::config::ProviderPreference::DirectMl { device_id } => {
                    crate::api::ProviderPreference::DirectMl { device_id }
                }
                crate::config::ProviderPreference::Cuda { device_id } => {
                    crate::api::ProviderPreference::Cuda { device_id }
                }
            },
            resolved: match resolution.resolved {
                crate::runtime::provider::ResolvedExecutionProvider::Cpu => {
                    crate::api::ResolvedProvider::Cpu
                }
                crate::runtime::provider::ResolvedExecutionProvider::DirectMl => {
                    crate::api::ResolvedProvider::DirectMl
                }
                crate::runtime::provider::ResolvedExecutionProvider::Cuda => {
                    crate::api::ResolvedProvider::Cuda
                }
            },
            fallback_to_cpu: resolution.fallback_used,
        };
        crate::api::ProviderInfo {
            detector: convert(resolutions.det),
            classifier: resolutions.cls.map(convert),
            recognizer: convert(resolutions.rec),
        }
    }
}

impl crate::api::OcrEngine for RapidOcrEngine {
    fn model_id(&self) -> &str {
        &self.model_id
    }
    fn provider(&self) -> crate::api::ProviderInfo {
        self.provider_info()
    }

    fn recognize(&mut self, request: crate::api::OcrRequest) -> Result<crate::api::OcrOutput> {
        request.validate()?;
        if request.formula.enabled {
            let policy = request.formula.clone();
            return self.recognize_with_formula(request, policy);
        }
        self.recognize_text(request)
    }
}

impl RapidOcrEngine {
    /// 公式路由关闭时的普通文本识别路径。
    ///
    /// 提取为独立函数是为了让“公式关闭时行为与不含公式功能时一致”成为结构性保证：
    /// 未启用公式时 `recognize` 直接走这里，不经过任何公式代码。
    fn recognize_text(&mut self, request: crate::api::OcrRequest) -> Result<crate::api::OcrOutput> {
        use crate::api::{
            ClassifierPolicy, ImageInput, OcrRegion, RegionSource, StageReport, StageState,
            StageTiming, TextOrientation, WordKind,
        };
        if let Some(tile) = request.preprocess.tile {
            return self.recognize_tiled_request(request, tile);
        }
        let preprocess_start = Instant::now();
        let roi_size = request.roi.map(|roi| crate::api::ImageSize {
            width: roi.width,
            height: roi.height,
        });
        let (input, original_size, offset) = match request.input {
            ImageInput::Pixels(view) => {
                ensure_decode_pixels(
                    view.width as usize,
                    view.height as usize,
                    request.preprocess.max_decode_pixels,
                )?;
                let (bgr, size, offset) = view.to_bgr(request.roi)?;
                (
                    OcrInput::BgrU8 {
                        width: size.width as usize,
                        height: size.height as usize,
                        data: bgr,
                    },
                    crate::api::ImageSize {
                        width: view.width,
                        height: view.height,
                    },
                    offset,
                )
            }
            ImageInput::Encoded(bytes) => {
                let image = self.inner.loader.load_with_limit(
                    OcrInput::Bytes(bytes.to_vec()),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                rec_image_input(image, request.roi, request.preprocess.max_decode_pixels)?
            }
            ImageInput::File(path) => {
                let image = self.inner.loader.load_with_limit(
                    OcrInput::Path(path),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                rec_image_input(image, request.roi, request.preprocess.max_decode_pixels)?
            }
            ImageInput::Url(url) => {
                let image = self.inner.loader.load_with_limit(
                    OcrInput::Url(url),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                rec_image_input(image, request.roi, request.preprocess.max_decode_pixels)?
            }
            ImageInput::Image(image) => {
                rec_image_input(image, request.roi, request.preprocess.max_decode_pixels)?
            }
        };
        let mut input = input;
        if matches!(
            request.preprocess.enhance,
            crate::api::EnhancementPolicy::ScreenAdaptive
        ) {
            let img = self.inner.loader.load(input)?;
            input = OcrInput::Image(crate::ocr::pipeline::image_ops::enhance_screen_adaptive(
                &img,
            )?);
        }
        let max_side = request
            .preprocess
            .max_side
            .unwrap_or(self.base_max_side_len as u32);
        self.inner
            .set_image_bounds(self.base_min_side_len as u32, max_side);
        let effective_scale = match (request.scale_hint, request.preprocess.min_text_scale) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        if let Some(scale) = effective_scale {
            let hinted = ((max_side as f32) * scale).round().clamp(32.0, 8192.0) as u32;
            let min_hint = ((self.base_min_side_len as f32) * scale)
                .round()
                .clamp(1.0, hinted as f32) as u32;
            self.inner.set_image_bounds(min_hint, hinted);
        }
        let mut use_cls = match request.stages.classify.policy {
            ClassifierPolicy::Off => false,
            ClassifierPolicy::IfAvailable => self.inner.classifier.is_some(),
            ClassifierPolicy::Required => true,
        };
        if use_cls && self.inner.classifier.is_none() {
            if matches!(request.stages.classify.policy, ClassifierPolicy::Required) {
                return Err(crate::error::RapidOcrError::Config(
                    "classification requested but no classifier model is configured".into(),
                ));
            }
            use_cls = false;
        }
        if !request.stages.detect && !request.stages.recognize && !use_cls {
            return Err(crate::error::RapidOcrError::InvalidInput(
                "requested stages are unavailable".into(),
            ));
        }
        let preprocess_ms = preprocess_start.elapsed().as_secs_f32() * 1000.0;
        let exec = self.inner.run(
            input,
            ExecutionOptions {
                use_det: request.stages.detect,
                use_cls,
                use_rec: request.stages.recognize,
                apply_cls_rotation: request.stages.classify.apply_rotation,
                return_word_box: !matches!(
                    request.recognition.words,
                    crate::api::WordOutputMode::Off
                ),
                return_single_char_box: matches!(
                    request.recognition.words,
                    crate::api::WordOutputMode::Chars
                ),
                text_score: request.detection.text_score,
                box_thresh: request.detection.box_thresh,
                unclip_ratio: request.detection.unclip_ratio,
            },
        )?;
        let local_source_size = roi_size.unwrap_or(original_size);
        let boxes = exec.boxes.clone().unwrap_or_default();
        let det_scores = exec.det_scores.clone().unwrap_or_default();
        let lines = exec.lines.clone().unwrap_or_default();
        let words = exec.word_boxes.clone().unwrap_or_default();
        let cls = exec.cls_res.clone().unwrap_or_default();
        let cls_count = cls.len();
        let mut regions = Vec::new();
        if request.stages.detect {
            for (i, det_box) in boxes.iter().enumerate() {
                let mut polygon = *det_box;
                for p in &mut polygon {
                    p[0] += offset.0 as f32;
                    p[1] += offset.1 as f32;
                }
                let recognition = lines.get(i).map(|line| crate::api::RecognitionOutcome {
                    text: line.text.clone(),
                    score: line.score,
                    words: words.get(i).map(|items| {
                        items
                            .iter()
                            .map(|w| crate::api::OcrWord {
                                text: w.text.clone(),
                                score: w.score,
                                polygon: crate::api::Polygon {
                                    points: w.bbox.map(|mut p| {
                                        p[0] += offset.0 as f32;
                                        p[1] += offset.1 as f32;
                                        p
                                    }),
                                },
                                kind: if matches!(
                                    request.recognition.words,
                                    crate::api::WordOutputMode::Chars
                                ) {
                                    WordKind::Char
                                } else {
                                    WordKind::Token
                                },
                            })
                            .collect()
                    }),
                });
                let classification =
                    cls.get(i)
                        .map(|(orientation, score)| crate::api::ClassificationOutcome {
                            orientation: if orientation == "180" {
                                TextOrientation::Deg180
                            } else {
                                TextOrientation::Deg0
                            },
                            score: *score,
                            applied_rotation: request.stages.classify.apply_rotation
                                && orientation == "180",
                        });
                regions.push(OcrRegion {
                    source: RegionSource::Detected { detector_index: i },
                    kind: crate::api::RegionKind::Text,
                    polygon: Some(crate::api::Polygon { points: polygon }),
                    detection: Some(crate::api::DetectionOutcome {
                        score: det_scores.get(i).copied().unwrap_or_default(),
                    }),
                    classification,
                    recognition,
                    formula: None,
                });
            }
        } else if request.stages.recognize {
            for (i, line) in lines.iter().enumerate() {
                let classification = cls.get(i).map(|(o, s)| crate::api::ClassificationOutcome {
                    orientation: if o == "180" {
                        TextOrientation::Deg180
                    } else {
                        TextOrientation::Deg0
                    },
                    score: *s,
                    applied_rotation: request.stages.classify.apply_rotation && o == "180",
                });
                regions.push(OcrRegion {
                    source: RegionSource::Input,
                    kind: crate::api::RegionKind::Text,
                    polygon: None,
                    detection: None,
                    classification,
                    recognition: Some(crate::api::RecognitionOutcome {
                        text: line.text.clone(),
                        score: line.score,
                        words: words.get(i).map(|items| {
                            items
                                .iter()
                                .map(|w| crate::api::OcrWord {
                                    text: w.text.clone(),
                                    score: w.score,
                                    polygon: crate::api::Polygon {
                                        points: w.bbox.map(|mut p| {
                                            let sx = exec
                                                .processed_size
                                                .map(|(w, _)| {
                                                    local_source_size.width as f32 / w.max(1) as f32
                                                })
                                                .unwrap_or(1.0);
                                            let sy = exec
                                                .processed_size
                                                .map(|(_, h)| {
                                                    local_source_size.height as f32
                                                        / h.max(1) as f32
                                                })
                                                .unwrap_or(1.0);
                                            p[0] = p[0] * sx + offset.0 as f32;
                                            p[1] = p[1] * sy + offset.1 as f32;
                                            p
                                        }),
                                    },
                                    kind: WordKind::Token,
                                })
                                .collect()
                        }),
                    }),
                    formula: None,
                });
            }
        } else if use_cls {
            for (o, s) in &cls {
                regions.push(OcrRegion {
                    source: RegionSource::Input,
                    kind: crate::api::RegionKind::Text,
                    polygon: None,
                    detection: None,
                    classification: Some(crate::api::ClassificationOutcome {
                        orientation: if o == "180" {
                            TextOrientation::Deg180
                        } else {
                            TextOrientation::Deg0
                        },
                        score: *s,
                        applied_rotation: request.stages.classify.apply_rotation && o == "180",
                    }),
                    recognition: None,
                    formula: None,
                });
            }
        }
        let timing = crate::api::OcrTimings {
            decode_ms: exec.decode_ms.unwrap_or_default(),
            resize_ms: exec.resize_ms.unwrap_or_default(),
            crop_ms: exec.crop_ms.unwrap_or_default(),
            preprocess_ms,
            detector_preprocess_ms: exec
                .det_breakdown_ms
                .map(|v| v.preprocess_ms)
                .unwrap_or_default(),
            detector_infer_ms: exec
                .det_breakdown_ms
                .map(|v| v.infer_ms)
                .unwrap_or_default(),
            detector_postprocess_ms: exec
                .det_breakdown_ms
                .map(|v| v.postprocess_ms)
                .unwrap_or_default(),
            detect_ms: exec.elapsed_ms[0].unwrap_or_default(),
            classifier_preprocess_ms: exec.cls_breakdown_ms.map(|v| v[0]).unwrap_or_default(),
            classifier_infer_ms: exec.cls_breakdown_ms.map(|v| v[1]).unwrap_or_default(),
            classifier_postprocess_ms: exec.cls_breakdown_ms.map(|v| v[2]).unwrap_or_default(),
            classify_ms: exec.elapsed_ms[1].unwrap_or_default(),
            recognizer_preprocess_ms: exec.rec_breakdown_ms.map(|v| v[0]).unwrap_or_default(),
            recognizer_infer_ms: exec.rec_breakdown_ms.map(|v| v[1]).unwrap_or_default(),
            recognizer_postprocess_ms: exec.rec_breakdown_ms.map(|v| v[2]).unwrap_or_default(),
            recognize_ms: exec.elapsed_ms[2].unwrap_or_default(),
            formula_ms: 0.0,
            postprocess_ms: exec.postprocess_ms.unwrap_or_default(),
            total_ms: exec.e2e_ms.unwrap_or_default() + preprocess_ms,
        };
        let detector = if !request.stages.detect {
            StageReport {
                state: StageState::Disabled,
                timing: None,
            }
        } else if boxes.is_empty() {
            StageReport {
                state: StageState::SkippedNoInput,
                timing: Some(StageTiming {
                    preprocess_ms: timing.detector_preprocess_ms,
                    infer_ms: timing.detector_infer_ms,
                    postprocess_ms: timing.detector_postprocess_ms,
                }),
            }
        } else {
            StageReport {
                state: StageState::Completed { items: boxes.len() },
                timing: Some(StageTiming {
                    preprocess_ms: timing.detector_preprocess_ms,
                    infer_ms: timing.detector_infer_ms,
                    postprocess_ms: timing.detector_postprocess_ms,
                }),
            }
        };
        let classifier = if matches!(request.stages.classify.policy, ClassifierPolicy::Off) {
            StageReport {
                state: StageState::Disabled,
                timing: None,
            }
        } else if self.inner.classifier.is_none() || !use_cls {
            StageReport {
                state: StageState::SkippedUnavailable,
                timing: None,
            }
        } else {
            StageReport {
                state: StageState::Completed { items: cls_count },
                timing: exec.cls_breakdown_ms.map(|v| StageTiming {
                    preprocess_ms: v[0],
                    infer_ms: v[1],
                    postprocess_ms: v[2],
                }),
            }
        };
        let recognizer = if !request.stages.recognize {
            StageReport {
                state: StageState::Disabled,
                timing: None,
            }
        } else if lines.is_empty() {
            StageReport {
                state: StageState::SkippedNoInput,
                timing: exec.rec_breakdown_ms.map(|v| StageTiming {
                    preprocess_ms: v[0],
                    infer_ms: v[1],
                    postprocess_ms: v[2],
                }),
            }
        } else {
            StageReport {
                state: StageState::Completed { items: lines.len() },
                timing: exec.rec_breakdown_ms.map(|v| StageTiming {
                    preprocess_ms: v[0],
                    infer_ms: v[1],
                    postprocess_ms: v[2],
                }),
            }
        };
        Ok(crate::api::OcrOutput {
            schema_version: 1,
            image: crate::api::ImageInfo {
                original_size,
                processed_size: exec
                    .processed_size
                    .map(|(width, height)| crate::api::ImageSize { width, height })
                    .unwrap_or(original_size),
                coordinate_space: request.output.coordinate_space,
            },
            stages: crate::api::StageReports {
                input: crate::api::InputTimings {
                    decode_ms: exec.decode_ms,
                    resize_ms: exec.resize_ms,
                    crop_ms: exec.crop_ms,
                },
                detector,
                classifier,
                recognizer,
                formula: crate::api::StageReport::default(),
            },
            regions,
            timings: timing,
            engine: crate::api::EngineInfo {
                model_id: self.model_id.clone(),
                provider: self.provider_info(),
            },
        })
    }
}

impl RapidOcrEngine {
    /// 页面级公式路由：检测/接收公式区域，抹白后跑普通文本管线，再单独识别公式。
    ///
    /// 顺序说明：
    ///
    /// 1. 用共享输入层解码**原图**（公式裁剪需要未抹白的像素）；
    /// 2. 解析公式区域（检测模型 + 显式区域，见 `formula::route`）；
    /// 3. 把公式区域抹白后交给普通文本管线：公式像素根本不进入检测/识别，
    ///    因此“跳过普通 CTC”是执行层面的跳过，而不是事后丢弃结果；
    /// 4. 从原图裁剪公式区域，批量送入 `FormulaRecognizer`；
    /// 5. 公式区域作为独立 `RegionKind::Formula` 追加到输出，携带 polygon 与模型标识。
    fn recognize_with_formula(
        &mut self,
        request: crate::api::OcrRequest,
        policy: crate::api::FormulaPolicy,
    ) -> Result<crate::api::OcrOutput> {
        use crate::api::{FormulaOutcome, ImageSize, OcrRegion, Polygon, RegionKind, RegionSource};

        let formula_start = Instant::now();
        let original = self.decode_formula_source(&request)?;
        let size = ImageSize {
            width: original.width(),
            height: original.height(),
        };

        let mut candidates = self.detect_formula_candidates(&original, &policy)?;
        for polygon in &policy.input_regions {
            polygon.validate(size)?;
            candidates.push(crate::formula::route::FormulaCandidate {
                polygon: *polygon,
                score: 1.0,
                detected: false,
            });
        }
        let regions = crate::formula::route::resolve_formula_regions(
            &candidates,
            size,
            policy.min_area_ratio,
            policy.iou_threshold,
            policy.max_regions,
        );
        let formula_ms = formula_start.elapsed().as_secs_f32() * 1000.0;

        let mut text_request = request.clone();
        text_request.formula = crate::api::FormulaPolicy::default();
        if !regions.is_empty() {
            // 抹白公式像素：普通文本管线看不到公式，也就不会对它们跑 CTC。
            text_request.input = crate::api::ImageInput::Image(
                crate::formula::route::whiten_regions(&original, &regions)?,
            );
            text_request.roi = None;
        }
        let mut output = self.recognize_text(text_request)?;

        if regions.is_empty() {
            output.stages.formula = crate::api::StageReport {
                state: crate::api::StageState::SkippedNoInput,
                timing: Some(crate::api::StageTiming {
                    preprocess_ms: formula_ms,
                    infer_ms: 0.0,
                    postprocess_ms: 0.0,
                }),
            };
            return Ok(output);
        }

        let crops: Vec<image::DynamicImage> = regions
            .iter()
            .map(|candidate| crop_polygon_bounds(&original, candidate.polygon))
            .collect();
        let recognize_start = Instant::now();
        let recognitions = self.formula_recognizer(&policy)?.recognize_batch(&crops)?;
        let recognize_ms = recognize_start.elapsed().as_secs_f32() * 1000.0;
        output.timings.formula_ms = recognize_ms + formula_ms;
        output.timings.total_ms += recognize_ms + formula_ms;

        for (index, (candidate, recognition)) in regions.iter().zip(recognitions).enumerate() {
            let detected = candidate.detected;
            let source = if detected {
                RegionSource::Detected {
                    detector_index: index,
                }
            } else {
                RegionSource::Input
            };
            let polygon = Some(Polygon {
                points: canonical_polygon_points(candidate.polygon),
            });
            output.regions.push(OcrRegion {
                source,
                kind: RegionKind::Formula,
                polygon,
                detection: detected.then_some(crate::api::DetectionOutcome {
                    score: candidate.score,
                }),
                classification: None,
                recognition: None,
                formula: Some(FormulaOutcome {
                    latex: recognition.latex,
                    eos_index: recognition.eos_index,
                    truncated: recognition.truncated,
                    model_id: recognition.model_id,
                    token_ids: policy.include_token_ids.then_some(recognition.token_ids),
                }),
            });
        }

        // `items` 是**公式阶段处理的公式数量**，不是页面区域总数：
        // 一页 20 个文本区域 + 2 个公式区域时必须报告 2。
        output.stages.formula = crate::api::StageReport {
            state: crate::api::StageState::Completed {
                items: output.formula_count(),
            },
            timing: Some(crate::api::StageTiming {
                preprocess_ms: formula_ms,
                infer_ms: recognize_ms,
                postprocess_ms: 0.0,
            }),
        };
        output.validate()?;
        Ok(output)
    }

    /// 解码请求输入为原图 `DynamicImage`，复用共享输入层的限制与错误语义。
    fn decode_formula_source(
        &self,
        request: &crate::api::OcrRequest,
    ) -> Result<image::DynamicImage> {
        use crate::api::ImageInput;
        let pixels = request.preprocess.max_decode_pixels;
        let encoded = request.preprocess.max_encoded_bytes;
        let image = match &request.input {
            ImageInput::Encoded(bytes) => self.inner.loader.load_dynamic_with_limit(
                OcrInput::Bytes(bytes.to_vec()),
                pixels,
                encoded,
            )?,
            ImageInput::File(path) => self.inner.loader.load_dynamic_with_limit(
                OcrInput::Path(path.clone()),
                pixels,
                encoded,
            )?,
            ImageInput::Url(url) => self.inner.loader.load_dynamic_with_limit(
                OcrInput::Url(url.clone()),
                pixels,
                encoded,
            )?,
            ImageInput::Pixels(view) => {
                view.validate()?;
                // 内存像素输入同样受 `max_decode_pixels` 约束：公式路由不能成为
                // 绕过普通 OCR 限制的旁路。
                ensure_decode_pixels(
                    view.width as usize,
                    view.height as usize,
                    request.preprocess.max_decode_pixels,
                )?;
                let (bgr, size, _) = view.to_bgr(None)?;
                let rec = crate::config::RecImage::from_bgr_u8(
                    size.width as usize,
                    size.height as usize,
                    bgr,
                )?;
                rec_image_to_dynamic(&rec)?
            }
            ImageInput::Image(rec) => {
                ensure_decode_pixels(
                    rec.width(),
                    rec.height(),
                    request.preprocess.max_decode_pixels,
                )?;
                rec_image_to_dynamic(rec)?
            }
        };
        Ok(image)
    }

    /// 运行公式检测模型；未配置检测模型时返回空候选（只处理显式区域）。
    fn detect_formula_candidates(
        &mut self,
        image: &image::DynamicImage,
        policy: &crate::api::FormulaPolicy,
    ) -> Result<Vec<crate::formula::route::FormulaCandidate>> {
        let Some(path) = policy.detector_path.clone() else {
            return Ok(Vec::new());
        };
        let options = crate::formula::detect::FormulaDetectOptions {
            confidence_threshold: policy.confidence_threshold,
            iou_threshold: policy.iou_threshold,
            max_detections: policy.max_regions,
        };
        let runtime = self.inner.config.rec.runtime.clone();
        if self
            .formula_detector
            .as_ref()
            .is_none_or(|(current, _)| *current != path)
        {
            self.formula_detector = Some((
                path.clone(),
                crate::formula::detect::FormulaDetector::from_model(&path, &runtime)?,
            ));
        }
        let detector = self
            .formula_detector
            .as_mut()
            .map(|(_, detector)| detector)
            .ok_or_else(|| {
                crate::error::RapidOcrError::Config("formula detector is not initialized".into())
            })?;
        Ok(detector
            .detect(image, &options)?
            .into_iter()
            .map(|detected| crate::formula::route::FormulaCandidate {
                polygon: crate::api::Polygon {
                    points: detected.polygon,
                },
                score: detected.score,
                detected: true,
            })
            .collect())
    }

    /// 懒加载公式识别器；模型路径变化时重建。
    fn formula_recognizer(
        &mut self,
        policy: &crate::api::FormulaPolicy,
    ) -> Result<&mut crate::formula::recognizer::FormulaRecognizer> {
        let path = policy.model_path.clone().ok_or_else(|| {
            crate::error::RapidOcrError::InvalidInput("formula policy requires `model_path`".into())
        })?;
        let runtime = self.inner.config.rec.runtime.clone();
        let rebuild = self
            .formula_recognizer
            .as_ref()
            .is_none_or(|(current, _)| *current != path);
        if rebuild {
            let recognizer = crate::formula::recognizer::FormulaRecognizer::from_model_with_hash(
                &path,
                &runtime,
                policy.expected_model_sha256.as_deref(),
            )?;
            self.formula_recognizer = Some((path, recognizer));
        }
        self.formula_recognizer
            .as_mut()
            .map(|(_, recognizer)| recognizer)
            .ok_or_else(|| {
                crate::error::RapidOcrError::Config("formula recognizer is not initialized".into())
            })
    }
}

/// 以多边形包围盒裁剪原图；旋转四边形同样使用轴对齐包围盒。
fn crop_polygon_bounds(
    image: &image::DynamicImage,
    polygon: crate::api::Polygon,
) -> image::DynamicImage {
    use image::GenericImageView;
    let (width, height) = image.dimensions();
    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for [x, y] in polygon.points {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let x0 = x0.floor().clamp(0.0, width as f32) as u32;
    let y0 = y0.floor().clamp(0.0, height as f32) as u32;
    let x1 = x1.ceil().clamp(0.0, width as f32) as u32;
    let y1 = y1.ceil().clamp(0.0, height as f32) as u32;
    let w = (x1.saturating_sub(x0))
        .max(1)
        .min(width.saturating_sub(x0).max(1));
    let h = (y1.saturating_sub(y0))
        .max(1)
        .min(height.saturating_sub(y0).max(1));
    image.crop_imm(x0, y0, w, h)
}

/// 输出的多边形统一为左上起顺时针四角，便于下游稳定消费。
fn canonical_polygon_points(polygon: crate::api::Polygon) -> [[f32; 2]; 4] {
    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for [x, y] in polygon.points {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

fn rec_image_to_dynamic(image: &crate::config::RecImage) -> Result<image::DynamicImage> {
    use crate::config::ColorOrder;
    let width = image.width() as u32;
    let height = image.height() as u32;
    let rgb: Vec<u8> = match image.color_order() {
        ColorOrder::Rgb => image.as_bytes().to_vec(),
        ColorOrder::Bgr => image
            .as_bytes()
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
            .collect(),
    };
    let buffer = image::RgbImage::from_raw(width, height, rgb).ok_or_else(|| {
        crate::error::RapidOcrError::InvalidImage("invalid RGB buffer for formula routing".into())
    })?;
    Ok(image::DynamicImage::ImageRgb8(buffer))
}

impl RapidOcrEngine {
    fn recognize_tiled_request(
        &mut self,
        request: crate::api::OcrRequest,
        tile: crate::api::TilePolicy,
    ) -> Result<crate::api::OcrOutput> {
        if matches!(
            request.stages.classify.policy,
            crate::api::ClassifierPolicy::Required
        ) && self.inner.classifier.is_none()
        {
            return Err(crate::error::RapidOcrError::Config(
                "classification requested but no classifier model is configured".into(),
            ));
        }
        if tile.max_width == 0
            || tile.max_height == 0
            || tile.overlap >= tile.max_width
            || tile.overlap >= tile.max_height
        {
            return Err(crate::error::RapidOcrError::InvalidInput(
                "invalid tile policy".into(),
            ));
        }
        let (rgba, original_size) = match request.input {
            crate::api::ImageInput::Encoded(bytes) => {
                let decoded = self.inner.loader.load_with_limit(
                    OcrInput::Bytes(bytes.to_vec()),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                let image = rec_image_to_rgba(&decoded)?;
                let size = crate::api::ImageSize {
                    width: image.width(),
                    height: image.height(),
                };
                (image, size)
            }
            crate::api::ImageInput::Pixels(view) => {
                view.validate()?;
                ensure_decode_pixels(
                    view.width as usize,
                    view.height as usize,
                    request.preprocess.max_decode_pixels,
                )?;
                let (bgr, size, _) = view.to_bgr(None)?;
                let rgb = crate::config::RecImage::from_bgr_u8(
                    size.width as usize,
                    size.height as usize,
                    bgr,
                )?
                .as_bytes()
                .to_vec();
                let mut rgba = vec![0_u8; size.width as usize * size.height as usize * 4];
                for (src, dst) in rgb
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(rgba.as_chunks_mut::<4>().0.iter_mut())
                {
                    dst[..3].copy_from_slice(&[src[2], src[1], src[0]]);
                    dst[3] = 255;
                }
                (
                    image::RgbaImage::from_raw(size.width, size.height, rgba).ok_or_else(|| {
                        crate::error::RapidOcrError::InvalidImage("invalid pixel buffer".into())
                    })?,
                    size,
                )
            }
            crate::api::ImageInput::File(path) => {
                let image = self.inner.loader.load_with_limit(
                    OcrInput::Path(path),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                let size = crate::api::ImageSize {
                    width: image.width() as u32,
                    height: image.height() as u32,
                };
                (rec_image_to_rgba(&image)?, size)
            }
            crate::api::ImageInput::Url(url) => {
                let image = self.inner.loader.load_with_limit(
                    OcrInput::Url(url),
                    request.preprocess.max_decode_pixels,
                    request.preprocess.max_encoded_bytes,
                )?;
                let size = crate::api::ImageSize {
                    width: image.width() as u32,
                    height: image.height() as u32,
                };
                (rec_image_to_rgba(&image)?, size)
            }
            crate::api::ImageInput::Image(image) => {
                let size = crate::api::ImageSize {
                    width: image.width() as u32,
                    height: image.height() as u32,
                };
                ensure_decode_pixels(
                    size.width as usize,
                    size.height as usize,
                    request.preprocess.max_decode_pixels,
                )?;
                (rec_image_to_rgba(&image)?, size)
            }
        };
        if let Some(roi) = request.roi {
            roi.validate_against(original_size.width, original_size.height)?;
        }
        let roi = request.roi.unwrap_or(crate::api::RectU32 {
            x: 0,
            y: 0,
            width: original_size.width,
            height: original_size.height,
        });
        let view = image::imageops::crop_imm(&rgba, roi.x, roi.y, roi.width, roi.height).to_image();
        let step_x = tile.max_width - tile.overlap;
        let step_y = tile.max_height - tile.overlap;
        let mut regions = Vec::new();
        let mut timings = crate::api::OcrTimings::default();
        let mut y = 0;
        while y < view.height() {
            let h = tile.max_height.min(view.height() - y);
            let mut x = 0;
            while x < view.width() {
                let w = tile.max_width.min(view.width() - x);
                let crop = image::imageops::crop_imm(&view, x, y, w, h).to_image();
                let mut preprocess = request.preprocess.clone();
                preprocess.tile = None;
                let sub = crate::api::OcrRequest {
                    input: crate::api::ImageInput::Pixels(crate::api::OwnedPixelBuffer {
                        width: w,
                        height: h,
                        stride: w as usize * 4,
                        format: crate::api::PixelFormat::Rgba8,
                        bottom_up: false,
                        data: Arc::from(crop.into_raw()),
                    }),
                    roi: None,
                    scale_hint: request.scale_hint,
                    stages: request.stages,
                    preprocess,
                    detection: request.detection,
                    recognition: request.recognition,
                    output: request.output,
                    // tiling 与公式路由互斥（`request.validate()` 已拒绝同时启用）。
                    formula: crate::api::FormulaPolicy::default(),
                };
                let mut out = self.recognize(sub)?;
                let region_offset = regions.len();
                timings.decode_ms += out.timings.decode_ms;
                timings.resize_ms += out.timings.resize_ms;
                timings.crop_ms += out.timings.crop_ms;
                timings.preprocess_ms += out.timings.preprocess_ms;
                timings.detector_preprocess_ms += out.timings.detector_preprocess_ms;
                timings.detector_infer_ms += out.timings.detector_infer_ms;
                timings.detector_postprocess_ms += out.timings.detector_postprocess_ms;
                timings.detect_ms += out.timings.detect_ms;
                timings.classifier_preprocess_ms += out.timings.classifier_preprocess_ms;
                timings.classifier_infer_ms += out.timings.classifier_infer_ms;
                timings.classifier_postprocess_ms += out.timings.classifier_postprocess_ms;
                timings.classify_ms += out.timings.classify_ms;
                timings.recognizer_preprocess_ms += out.timings.recognizer_preprocess_ms;
                timings.recognizer_infer_ms += out.timings.recognizer_infer_ms;
                timings.recognizer_postprocess_ms += out.timings.recognizer_postprocess_ms;
                timings.recognize_ms += out.timings.recognize_ms;
                timings.postprocess_ms += out.timings.postprocess_ms;
                for region in &mut out.regions {
                    if let Some(poly) = &mut region.polygon {
                        for p in &mut poly.points {
                            p[0] += (roi.x + x) as f32;
                            p[1] += (roi.y + y) as f32;
                        }
                    }
                    if let Some(rec) = &mut region.recognition
                        && let Some(words) = &mut rec.words
                    {
                        for word in words {
                            for p in &mut word.polygon.points {
                                p[0] += (roi.x + x) as f32;
                                p[1] += (roi.y + y) as f32;
                            }
                        }
                    }
                    if let crate::api::RegionSource::Detected { detector_index } = region.source {
                        region.source = crate::api::RegionSource::Detected {
                            detector_index: detector_index + region_offset,
                        };
                    }
                }
                regions.extend(out.regions);
                if x + w >= view.width() {
                    break;
                }
                x += step_x;
            }
            if y + h >= view.height() {
                break;
            }
            y += step_y;
        }
        regions = deduplicate_regions(regions);
        timings.total_ms = timings.preprocess_ms
            + timings.detect_ms
            + timings.classify_ms
            + timings.recognize_ms
            + timings.postprocess_ms;
        Ok(crate::api::OcrOutput {
            schema_version: 1,
            image: crate::api::ImageInfo {
                original_size,
                processed_size: original_size,
                coordinate_space: request.output.coordinate_space,
            },
            stages: crate::api::StageReports {
                input: crate::api::InputTimings {
                    decode_ms: Some(timings.decode_ms),
                    resize_ms: Some(timings.resize_ms),
                    crop_ms: Some(timings.crop_ms),
                },
                detector: crate::api::StageReport {
                    state: if !request.stages.detect {
                        crate::api::StageState::Disabled
                    } else {
                        let items = regions.iter().filter(|r| r.detection.is_some()).count();
                        if items == 0 {
                            crate::api::StageState::SkippedNoInput
                        } else {
                            crate::api::StageState::Completed { items }
                        }
                    },
                    timing: request.stages.detect.then_some(crate::api::StageTiming {
                        preprocess_ms: timings.detector_preprocess_ms,
                        infer_ms: timings.detector_infer_ms,
                        postprocess_ms: timings.detector_postprocess_ms,
                    }),
                },
                classifier: crate::api::StageReport {
                    state: if matches!(
                        request.stages.classify.policy,
                        crate::api::ClassifierPolicy::Off
                    ) {
                        crate::api::StageState::Disabled
                    } else if self.inner.classifier.is_none() {
                        crate::api::StageState::SkippedUnavailable
                    } else {
                        let items = regions
                            .iter()
                            .filter(|r| r.classification.is_some())
                            .count();
                        if items == 0 {
                            crate::api::StageState::SkippedNoInput
                        } else {
                            crate::api::StageState::Completed { items }
                        }
                    },
                    timing: if matches!(
                        request.stages.classify.policy,
                        crate::api::ClassifierPolicy::Off
                    ) || self.inner.classifier.is_none()
                    {
                        None
                    } else {
                        Some(crate::api::StageTiming {
                            preprocess_ms: timings.classifier_preprocess_ms,
                            infer_ms: timings.classifier_infer_ms,
                            postprocess_ms: timings.classifier_postprocess_ms,
                        })
                    },
                },
                recognizer: crate::api::StageReport {
                    state: if !request.stages.recognize {
                        crate::api::StageState::Disabled
                    } else {
                        let items = regions.iter().filter(|r| r.recognition.is_some()).count();
                        if items == 0 {
                            crate::api::StageState::SkippedNoInput
                        } else {
                            crate::api::StageState::Completed { items }
                        }
                    },
                    timing: request.stages.recognize.then_some(crate::api::StageTiming {
                        preprocess_ms: timings.recognizer_preprocess_ms,
                        infer_ms: timings.recognizer_infer_ms,
                        postprocess_ms: timings.recognizer_postprocess_ms,
                    }),
                },
                // 分块路径与公式路由互斥（`request.validate()` 已拒绝同时启用）。
                formula: crate::api::StageReport::default(),
            },
            regions,
            timings,
            engine: crate::api::EngineInfo {
                model_id: self.model_id.clone(),
                provider: self.provider_info(),
            },
        })
    }
}

fn deduplicate_regions(mut regions: Vec<crate::api::OcrRegion>) -> Vec<crate::api::OcrRegion> {
    let mut unique = Vec::with_capacity(regions.len());
    for region in regions.drain(..) {
        let duplicate = region.recognition.as_ref().and_then(|recognition| {
            region.polygon.and_then(|polygon| {
                unique.iter().position(|existing: &crate::api::OcrRegion| {
                    existing
                        .recognition
                        .as_ref()
                        .is_some_and(|v| v.text == recognition.text)
                        && existing.polygon.is_some_and(|other| {
                            crate::evaluation::ocr::polygon_iou(other.points, polygon.points) >= 0.5
                        })
                })
            })
        });
        if let Some(index) = duplicate {
            let replace = region
                .recognition
                .as_ref()
                .zip(unique[index].recognition.as_ref())
                .is_some_and(|(a, b)| a.score > b.score);
            if replace {
                unique[index] = region;
            }
        } else {
            unique.push(region);
        }
    }
    unique.sort_by(|a, b| {
        let key = |region: &crate::api::OcrRegion| {
            region
                .polygon
                .map(|polygon| {
                    (
                        polygon
                            .points
                            .iter()
                            .map(|p| p[1])
                            .fold(f32::INFINITY, f32::min),
                        polygon
                            .points
                            .iter()
                            .map(|p| p[0])
                            .fold(f32::INFINITY, f32::min),
                    )
                })
                .unwrap_or((f32::INFINITY, f32::INFINITY))
        };
        key(a)
            .partial_cmp(&key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    unique
}

#[cfg(test)]
mod formula_integration_tests {
    //! 页面级公式路由的集成测试。
    //!
    //! 这些测试需要真实模型（普通 OCR、公式识别、公式检测）与真实页面图片，
    //! 因此全部通过环境变量定位：缺失时显式 skip，绝不 panic、绝不回落到
    //! 开发机绝对路径。

    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::api::{
        ClassifierPlan, ClassifierPolicy, DetectionPolicy, FormulaPolicy, ImageInput, OcrEngine,
        OcrRequest, OutputPolicy, PreprocessPolicy, RecognitionPolicy, RegionKind, RegionSource,
        StagePlan, StageState, TextOrder, WordOutputMode,
    };
    use crate::ocr::pipeline::config::EngineConfig;
    use crate::ocr::pipeline::rapid_ocr::RapidOcrEngine;

    fn engine_config(model_root: &std::path::Path) -> EngineConfig {
        let yaml = format!(
            r#"
global:
  use_det: true
  use_cls: false
  use_rec: true
  max_side_len: 2000
  min_side_len: 30
det:
  lang: multi
  ocr_version: PP-OCRv6
  model_type: small
  model_path: {root}/small/PP-OCRv6_det_small.onnx
  allow_download: false
cls:
  lang: ch
rec:
  model:
    lang: ch
    ocr_version: PP-OCRv6
    model_type: small
    model_path: {root}/small/PP-OCRv6_rec_small.onnx
    rec_keys_path: {root}/small/ppocrv6_dict.txt
    allow_download: false
"#,
            root = model_root.display().to_string().replace('\\', "/")
        );
        EngineConfig::from_yaml_str(&yaml).expect("engine config template must parse")
    }

    struct Assets {
        engine: RapidOcrEngine,
        formula_model: PathBuf,
        detector: PathBuf,
        page_with_formula: PathBuf,
        /// 实测在该页面上检测器返回 0 个候选（无公式 / 漏检场景）。
        page_without_detections: PathBuf,
        /// 实测该页面没有公式，但默认阈值下检测器会误检（用于验证阈值可控性）。
        page_with_false_positives: PathBuf,
    }

    fn assets() -> Option<Assets> {
        let model_root = crate::test_support::ocr_model_root()?;
        let formula_model = crate::test_support::formula_model_path()?;
        let detector = crate::test_support::formula_detector_path()?;
        let page_with_formula = crate::test_support::page_fixture("08数字公式与符号.png")?;
        let page_without_detections = crate::test_support::page_fixture("09竖排文本.png")?;
        let page_with_false_positives = crate::test_support::page_fixture("05代码与等宽字体.png")?;
        let engine = RapidOcrEngine::new(engine_config(&model_root)).expect("engine should load");
        Some(Assets {
            engine,
            formula_model,
            detector,
            page_with_formula,
            page_without_detections,
            page_with_false_positives,
        })
    }

    fn request(bytes: Vec<u8>, formula: FormulaPolicy) -> OcrRequest {
        OcrRequest {
            input: ImageInput::Encoded(Arc::from(bytes)),
            roi: None,
            scale_hint: None,
            stages: StagePlan {
                detect: true,
                classify: ClassifierPlan {
                    policy: ClassifierPolicy::Off,
                    apply_rotation: false,
                },
                recognize: true,
            },
            preprocess: PreprocessPolicy::default(),
            detection: DetectionPolicy::default(),
            recognition: RecognitionPolicy {
                words: WordOutputMode::Off,
            },
            output: OutputPolicy::default(),
            formula,
        }
    }

    fn formula_policy(assets: &Assets) -> FormulaPolicy {
        FormulaPolicy {
            enabled: true,
            model_path: Some(assets.formula_model.clone()),
            detector_path: Some(assets.detector.clone()),
            ..FormulaPolicy::default()
        }
    }

    /// 公式路由与 `roi`/`tile` 互斥，必须在进入管线前结构化拒绝。
    #[test]
    fn formula_routing_rejects_roi_and_tile() {
        let base = FormulaPolicy {
            enabled: true,
            model_path: Some(PathBuf::from("model.onnx")),
            ..FormulaPolicy::default()
        };
        let mut with_roi = request(Vec::new(), base.clone());
        with_roi.roi = Some(crate::api::RectU32 {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        });
        let error = with_roi.validate().expect_err("roi must be rejected");
        assert!(error.to_string().contains("roi"), "error: {error}");

        let mut with_tile = request(Vec::new(), base);
        with_tile.preprocess.tile = Some(crate::api::TilePolicy {
            max_width: 100,
            max_height: 100,
            overlap: 10,
        });
        let error = with_tile.validate().expect_err("tile must be rejected");
        assert!(error.to_string().contains("tiled"), "error: {error}");
    }

    #[test]
    fn formula_policy_requires_a_model_path() {
        let policy = FormulaPolicy {
            enabled: true,
            ..FormulaPolicy::default()
        };
        assert!(policy.validate().is_err());
    }

    /// 公式功能关闭时：输出中不能出现公式区域，公式阶段必须是 `Disabled`。
    #[test]
    fn disabled_formula_policy_produces_no_formula_regions() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_with_formula).expect("page readable");
        let output = assets
            .engine
            .recognize(request(bytes, FormulaPolicy::default()))
            .expect("baseline recognize");
        assert!(output.validate().is_ok());
        assert_eq!(output.formula_count(), 0);
        assert!(output.formula_latex(TextOrder::Reading).is_empty());
        assert_eq!(output.stages.formula.state, StageState::Disabled);
        assert!(
            output
                .regions
                .iter()
                .all(|region| region.kind == RegionKind::Text),
            "disabled formula policy must not produce formula regions"
        );
    }

    /// 漏检场景：页面上没有公式（检测器返回 0 候选）时，公式路由不得改变文本输出。
    #[test]
    fn formula_routing_without_detections_matches_the_baseline() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_without_detections).expect("page readable");
        let baseline = assets
            .engine
            .recognize(request(bytes.clone(), FormulaPolicy::default()))
            .expect("baseline recognize");
        let routed = assets
            .engine
            .recognize(request(bytes, formula_policy(&assets)))
            .expect("routed recognize");
        assert_eq!(routed.formula_count(), 0, "no formula on this page");
        assert_eq!(routed.stages.formula.state, StageState::SkippedNoInput);
        assert_eq!(
            routed.plain_text(TextOrder::Reading),
            baseline.plain_text(TextOrder::Reading),
            "formula routing must not change text output when nothing is detected"
        );
        assert_eq!(
            routed.regions.len(),
            baseline.regions.len(),
            "region count must not change when nothing is detected"
        );
    }

    /// 误检可控性：在实测会误检的页面上，提高置信度阈值必须能恢复到完全基线行为。
    ///
    /// 这条测试同时把“检测器在非公式页面上会有误检”这一事实固定下来：
    /// 调用方必须通过阈值/面积下限在召回与精度之间取舍，而不是假设检测器永远正确。
    #[test]
    fn strict_confidence_threshold_restores_baseline_on_a_noisy_page() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_with_false_positives).expect("page readable");
        let baseline = assets
            .engine
            .recognize(request(bytes.clone(), FormulaPolicy::default()))
            .expect("baseline recognize");

        let default_threshold = assets
            .engine
            .recognize(request(bytes.clone(), formula_policy(&assets)))
            .expect("routed recognize");
        assert!(
            default_threshold.formula_count() > 0,
            "this page is known to produce false positives at the default threshold; \
             if that changed, update this test and the documented limitation"
        );

        let strict = FormulaPolicy {
            confidence_threshold: 0.95,
            ..formula_policy(&assets)
        };
        let strict_output = assets
            .engine
            .recognize(request(bytes, strict))
            .expect("strict recognize");
        assert_eq!(strict_output.formula_count(), 0);
        assert_eq!(
            strict_output.stages.formula.state,
            StageState::SkippedNoInput
        );
        assert_eq!(
            strict_output.plain_text(TextOrder::Reading),
            baseline.plain_text(TextOrder::Reading),
            "with a strict threshold the output must equal the formula-disabled baseline"
        );
    }

    /// 主路径：检测到公式后，公式区域是独立 typed region，且文本区域不含公式文本。
    #[test]
    fn detected_formula_regions_are_typed_and_carry_latex() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_with_formula).expect("page readable");
        let output = assets
            .engine
            .recognize(request(bytes, formula_policy(&assets)))
            .expect("routed recognize");
        output.validate().expect("routed output must validate");
        assert!(
            output.formula_count() > 0,
            "the formula page must produce at least one formula region"
        );
        assert_eq!(
            output.stages.formula.state,
            StageState::Completed {
                items: output.formula_count()
            },
            "the formula stage must count formula regions, not all regions"
        );
        assert!(
            output.formula_count() < output.regions.len(),
            "this page has text regions too, so the two counts must differ"
        );
        for region in output
            .regions
            .iter()
            .filter(|region| region.kind == RegionKind::Formula)
        {
            let formula = region.formula.as_ref().expect("formula outcome");
            assert!(
                region.recognition.is_none(),
                "formula regions must not carry CTC text: {:?}",
                region.recognition
            );
            assert!(!formula.model_id.is_empty());
            assert!(
                region.polygon.is_some(),
                "formula regions must keep their crop polygon"
            );
            assert!(
                formula.token_ids.is_none(),
                "token ids must stay opt-in outside debug mode"
            );
        }
    }

    /// 显式声明的公式区域无需检测模型即可工作，并带上 `RegionSource::Input`。
    #[test]
    fn explicit_input_regions_are_recognized_without_a_detector() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_with_formula).expect("page readable");
        let policy = FormulaPolicy {
            enabled: true,
            model_path: Some(assets.formula_model.clone()),
            detector_path: None,
            input_regions: vec![crate::api::Polygon {
                points: [
                    [120.0, 420.0],
                    [900.0, 420.0],
                    [900.0, 560.0],
                    [120.0, 560.0],
                ],
            }],
            include_token_ids: true,
            ..FormulaPolicy::default()
        };
        let output = assets
            .engine
            .recognize(request(bytes, policy))
            .expect("explicit region recognize");
        output.validate().expect("output must validate");
        assert_eq!(output.formula_count(), 1);
        let region = output
            .regions
            .iter()
            .find(|region| region.kind == RegionKind::Formula)
            .expect("formula region");
        assert_eq!(region.source, RegionSource::Input);
        assert!(
            region.detection.is_none(),
            "explicit regions have no detector score"
        );
        let formula = region.formula.as_ref().expect("formula outcome");
        assert!(
            formula.token_ids.is_some(),
            "include_token_ids must keep the raw token sequence"
        );
    }

    /// 内存像素/内存图像输入必须与编码输入一样受 `max_decode_pixels` 约束。
    ///
    /// 公式路由不能成为绕过解码像素上限的旁路。
    #[test]
    fn formula_routing_enforces_the_pixel_limit_for_in_memory_inputs() {
        let Some(mut assets) = assets() else {
            return;
        };
        let page = image::open(&assets.page_with_formula).expect("page readable");
        let (width, height) = (page.width(), page.height());
        let tight_limit = u64::from(width) * u64::from(height) - 1;

        // 1) 编码输入（File）——基线：已经受限制。
        let mut from_file = request(Vec::new(), formula_policy(&assets));
        from_file.input = ImageInput::File(assets.page_with_formula.clone());
        from_file.preprocess.max_decode_pixels = tight_limit;
        let error = assets
            .engine
            .recognize(from_file)
            .expect_err("encoded input must respect the pixel limit");
        assert!(
            matches!(error, crate::error::RapidOcrError::InvalidImage(_)),
            "unexpected error: {error}"
        );

        // 2) 内存像素输入（Pixels）——回归：曾经绕过限制。
        let rgb = page.to_rgb8();
        let mut from_pixels = request(Vec::new(), formula_policy(&assets));
        from_pixels.input = ImageInput::Pixels(crate::api::OwnedPixelBuffer {
            data: Arc::from(rgb.as_raw().as_slice()),
            width,
            height,
            stride: (width * 3) as usize,
            format: crate::api::PixelFormat::Rgb8,
            bottom_up: false,
        });
        from_pixels.preprocess.max_decode_pixels = tight_limit;
        let error = assets
            .engine
            .recognize(from_pixels)
            .expect_err("in-memory pixel input must respect the pixel limit");
        assert!(
            matches!(error, crate::error::RapidOcrError::InvalidImage(_)),
            "unexpected error: {error}"
        );

        // 3) 内存图像输入（Image）——回归：曾经绕过限制。
        let rec = crate::config::RecImage::from_bgr_u8(width as usize, height as usize, {
            let mut bgr = Vec::with_capacity((width * height * 3) as usize);
            for pixel in rgb.pixels() {
                bgr.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            bgr
        })
        .expect("rec image");
        let mut from_image = request(Vec::new(), formula_policy(&assets));
        from_image.input = ImageInput::Image(rec);
        from_image.preprocess.max_decode_pixels = tight_limit;
        let error = assets
            .engine
            .recognize(from_image)
            .expect_err("in-memory image input must respect the pixel limit");
        assert!(
            matches!(error, crate::error::RapidOcrError::InvalidImage(_)),
            "unexpected error: {error}"
        );

        // 放宽限制后同一条内存输入必须能正常跑完，证明上面的失败来自限制而不是输入本身。
        let rgb_again = page.to_rgb8();
        let mut allowed = request(Vec::new(), formula_policy(&assets));
        allowed.input = ImageInput::Pixels(crate::api::OwnedPixelBuffer {
            data: Arc::from(rgb_again.as_raw().as_slice()),
            width,
            height,
            stride: (width * 3) as usize,
            format: crate::api::PixelFormat::Rgb8,
            bottom_up: false,
        });
        allowed.preprocess.max_decode_pixels = u64::from(width) * u64::from(height);
        let output = assets
            .engine
            .recognize(allowed)
            .expect("pixel input within the limit must succeed");
        output.validate().expect("output must validate");
    }

    /// 抹白是**按区域**进行的：区域内的文本消失，远处文本的内容仍然保留。
    ///
    /// 这条测试把 `FormulaPolicy` 文档里描述的“整区域抹白”语义固定下来：
    /// 它不是“按文本框覆盖比例跳过 CTC”，而是先移除区域像素再跑文本管线。
    ///
    /// 同时固定两个实测事实（`tests/` 中没有对应断言，因此写在这里）：
    ///
    /// 1. 抹白改变了文本检测的输入，因此**远处**区域的**分割**也可能变化
    ///    （实测：“42.7 ms” 被重新切成 “42.7” 与 “ms” 两个区域），所以断言按
    ///    去除空白后的内容比较，而不是要求区域边界不变；
    /// 2. 区域**内部**的文本不再出现在文本通道里——这是抹白的核心语义。
    #[test]
    fn whitening_removes_text_inside_the_region_and_keeps_distant_text() {
        let Some(mut assets) = assets() else {
            return;
        };
        let bytes = std::fs::read(&assets.page_with_formula).expect("page readable");
        let baseline = assets
            .engine
            .recognize(request(bytes.clone(), FormulaPolicy::default()))
            .expect("baseline recognize");
        let text_regions: Vec<_> = baseline
            .regions
            .iter()
            .filter(|region| region.kind == RegionKind::Text)
            .filter_map(|region| {
                let polygon = region.polygon?;
                let text = region.recognition.as_ref()?.text.clone();
                (!text.trim().is_empty()).then_some((polygon, text))
            })
            .collect();
        assert!(
            text_regions.len() >= 3,
            "baseline must produce several non-empty text regions"
        );

        let centroid = |polygon: &crate::api::Polygon| {
            let points = polygon.points;
            let sum_x: f32 = points.iter().map(|point| point[0]).sum();
            let sum_y: f32 = points.iter().map(|point| point[1]).sum();
            (sum_x / points.len() as f32, sum_y / points.len() as f32)
        };
        let compact = |text: &str| {
            text.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };

        // 取中间一个文本框作为显式公式区域，再挑出离它最远的文本框。
        let (target_polygon, target_text) = text_regions[text_regions.len() / 2].clone();
        let target_centroid = centroid(&target_polygon);
        let (distant_polygon, distant_text) = text_regions
            .iter()
            .max_by(|left, right| {
                let distance = |candidate: &(crate::api::Polygon, String)| {
                    let (x, y) = centroid(&candidate.0);
                    (x - target_centroid.0).hypot(y - target_centroid.1)
                };
                distance(left)
                    .partial_cmp(&distance(right))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("at least one distant region")
            .clone();
        assert_ne!(distant_polygon, target_polygon);

        let policy = FormulaPolicy {
            enabled: true,
            model_path: Some(assets.formula_model.clone()),
            detector_path: None,
            input_regions: vec![target_polygon],
            ..FormulaPolicy::default()
        };
        let routed = assets
            .engine
            .recognize(request(bytes, policy))
            .expect("routed recognize");
        let routed_text = compact(&routed.plain_text(TextOrder::Reading));

        assert!(
            !routed_text.contains(&compact(&target_text)),
            "text covered by the whitened region must disappear: {target_text:?}"
        );
        assert!(
            routed_text.contains(&compact(&distant_text)),
            "content far from the whitened region must survive (whitespace-insensitive, because \
             whitening can re-segment distant regions): {distant_text:?}"
        );
        assert_eq!(
            routed.formula_count(),
            1,
            "the explicit region must still be recognized as a formula"
        );
    }
}

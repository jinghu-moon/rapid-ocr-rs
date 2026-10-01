use std::time::Instant;
use std::{
    sync::{Arc, Once},
    thread,
};

use crate::{
    api::OcrEngine as _,
    cls::classifier::{Classifier, ClassifierConfig},
    config::RecognizeOptions,
    det::detector::{Detector, DetectorConfig},
    error::Result,
    input::image_loader::{LoadImage, OcrInput},
    pipeline::{
        config::EngineConfig,
        image_ops::{
            PreprocessRecord, apply_vertical_padding, crop_text_regions, map_boxes_to_original,
            map_img_to_original, resize_image_within_bounds,
        },
        types::{ExecutionOptions, ExecutionOutput},
    },
    rec::recognizer::Recognizer,
    runtime::provider::ProviderResolution,
    types::{LineResult, WordBox},
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
            let word_boxes = crate::rec::word_boxes::compute_word_boxes_with_backend(
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

fn ensure_decode_pixels(width: u32, height: u32, max_decode_pixels: u64) -> Result<()> {
    let pixels = (width as u64).checked_mul(height as u64).ok_or_else(|| {
        crate::error::RapidOcrError::InvalidInput("image dimensions overflow".into())
    })?;
    if pixels > max_decode_pixels {
        return Err(crate::error::RapidOcrError::InvalidImage(format!(
            "image has {pixels} pixels, limit is {max_decode_pixels}"
        )));
    }
    Ok(())
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
    ensure_decode_pixels(original_size.width, original_size.height, max_decode_pixels)?;
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
                crate::config::ProviderPreference::Cann { device_id } => {
                    crate::api::ProviderPreference::Cann { device_id }
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
                crate::runtime::provider::ResolvedExecutionProvider::Cann => {
                    crate::api::ResolvedProvider::Cann
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
        use crate::api::{
            ClassifierPolicy, ImageInput, OcrRegion, RegionSource, StageReport, StageState,
            StageTiming, TextOrientation, WordKind,
        };
        request.validate()?;
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
                    view.width,
                    view.height,
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
            input = OcrInput::Image(crate::pipeline::image_ops::enhance_screen_adaptive(&img)?);
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
            for i in 0..boxes.len() {
                let mut polygon = boxes[i];
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
                    polygon: Some(crate::api::Polygon { points: polygon }),
                    detection: Some(crate::api::DetectionOutcome {
                        score: det_scores.get(i).copied().unwrap_or_default(),
                    }),
                    classification,
                    recognition,
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
                });
            }
        } else if use_cls {
            for (o, s) in &cls {
                regions.push(OcrRegion {
                    source: RegionSource::Input,
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
        } else if self.inner.classifier.is_none() {
            StageReport {
                state: StageState::SkippedUnavailable,
                timing: None,
            }
        } else if !use_cls {
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
                    view.width,
                    view.height,
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
                    size.width,
                    size.height,
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
                    if let Some(rec) = &mut region.recognition {
                        if let Some(words) = &mut rec.words {
                            for word in words {
                                for p in &mut word.polygon.points {
                                    p[0] += (roi.x + x) as f32;
                                    p[1] += (roi.y + y) as f32;
                                }
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
                            crate::evaluation::polygon_iou(other.points, polygon.points) >= 0.5
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

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use ndarray::ArrayView4;
use rayon::prelude::*;

use crate::{
    config::{LangRec, RecImage, RuntimeConfig},
    error::{RapidOcrError, Result},
    model_registry::{ModelRegistry, ResolvedRecModel},
    model_store::{
        DownloadRequest, default_model_store_dir, download_verified, require_model_hash,
        verify_existing_file,
    },
    ocr::config::{RecognizeOptions, RecognizerConfig},
    ocr::rec::{
        bidi::reorder_bidi_for_display,
        decode::CtcLabelDecoder,
        preprocess::{batch_shape_for, write_resize_norm_img_into_slice_with_scratch},
    },
    ocr::session::{OcrSession, OcrSessionKind},
    ocr::types::{LineResult, RecognizeOutput},
    runtime::provider::ProviderResolution,
    vision::resize::LinearResizeScratch,
};

#[derive(Debug)]
pub struct Recognizer {
    config: RecognizerConfig,
    session: OcrSession,
    decoder: CtcLabelDecoder,
    batch_scratch: Vec<f32>,
}

impl Recognizer {
    /// `runtime` 来自引擎唯一的运行时档案（`RuntimeProfile::session_runtime`）。
    pub fn new(config: RecognizerConfig, runtime: &RuntimeConfig) -> Result<Self> {
        if config.rec_img_shape[0] != 3 {
            return Err(RapidOcrError::Config(format!(
                "rec_img_shape must start with channel=3, got {:?}",
                config.rec_img_shape
            )));
        }
        if config.rec_batch_num == 0 {
            return Err(RapidOcrError::Config(
                "rec_batch_num must be greater than zero".to_string(),
            ));
        }

        let model_store_dir = config
            .model_store_dir
            .clone()
            .unwrap_or_else(default_model_store_dir);

        let registry = ModelRegistry::from_default_yaml()?;
        let resolved = registry.resolve_rec(
            config.model.ocr_version,
            config.model.lang,
            config.model.model_type,
        )?;

        let model_path = resolve_model_path(&config, &resolved, &model_store_dir)?;
        let mut session = OcrSession::new(&model_path, runtime, OcrSessionKind::Rec)?;

        let character = session.take_character_list();
        let character_path = if character.is_none() {
            resolve_character_path(&config, &resolved, &model_store_dir)?
        } else {
            config.model.rec_keys_path.clone()
        };

        let decoder = CtcLabelDecoder::new(character, character_path.as_deref())?;

        Ok(Self {
            config,
            session,
            decoder,
            batch_scratch: Vec::new(),
        })
    }

    pub fn recognize(
        &mut self,
        images: &[RecImage],
        options: RecognizeOptions,
    ) -> Result<RecognizeOutput> {
        let start = Instant::now();

        if images.is_empty() {
            return Ok(RecognizeOutput::default());
        }

        let width_list: Vec<f64> = images
            .iter()
            .map(|img| img.width() as f64 / img.height() as f64)
            .collect();
        let mut indices: Vec<usize> = (0..images.len()).collect();
        indices.sort_by(|a, b| {
            width_list[*a]
                .partial_cmp(&width_list[*b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut rec_res: Vec<Option<LineResult>> = vec![None; images.len()];
        let mut preprocess_ms = 0.0_f32;
        let mut infer_ms = 0.0_f32;
        let mut postprocess_ms = 0.0_f32;

        for beg in (0..images.len()).step_by(self.config.rec_batch_num) {
            let end = (beg + self.config.rec_batch_num).min(images.len());
            let batch_indices = &indices[beg..end];

            let img_h = self.config.rec_img_shape[1] as f64;
            let img_w = self.config.rec_img_shape[2] as f64;
            let mut max_wh_ratio = img_w / img_h;

            let mut wh_ratio_list = Vec::with_capacity(end - beg);
            for sorted_idx in batch_indices {
                let img = &images[*sorted_idx];
                let wh_ratio = img.width() as f64 / img.height() as f64;
                max_wh_ratio = max_wh_ratio.max(wh_ratio);
                wh_ratio_list.push(wh_ratio as f32);
            }

            let (img_channel, img_height, dst_width) =
                batch_shape_for(max_wh_ratio, self.config.rec_img_shape)?;
            let sample_len = img_channel
                .checked_mul(img_height)
                .and_then(|v| v.checked_mul(dst_width))
                .ok_or_else(|| {
                    RapidOcrError::InvalidInput("rec batch sample size overflow".to_string())
                })?;
            let batch_size = batch_indices.len();
            let total_len = sample_len.checked_mul(batch_size).ok_or_else(|| {
                RapidOcrError::InvalidInput("rec batch size overflow".to_string())
            })?;
            self.batch_scratch.resize(total_len, 0.0);

            let preprocess_start = Instant::now();
            if batch_size > 1 {
                self.batch_scratch[..total_len]
                    .par_chunks_mut(sample_len)
                    .zip(batch_indices.par_iter().copied())
                    .try_for_each_init(
                        || (Vec::<u8>::new(), LinearResizeScratch::default()),
                        |(tmp_bgr, resize_scratch), (dst, image_idx)| {
                            let image = images.get(image_idx).ok_or_else(|| {
                                RapidOcrError::InvalidInput(format!(
                                    "batch index {image_idx} out of bounds for image count {}",
                                    images.len()
                                ))
                            })?;
                            write_resize_norm_img_into_slice_with_scratch(
                                image,
                                max_wh_ratio,
                                self.config.rec_img_shape,
                                dst,
                                tmp_bgr,
                                resize_scratch,
                            )
                        },
                    )?;
            } else {
                let image_idx = batch_indices[0];
                let image = images.get(image_idx).ok_or_else(|| {
                    RapidOcrError::InvalidInput(format!(
                        "batch index {image_idx} out of bounds for image count {}",
                        images.len()
                    ))
                })?;
                let mut tmp_bgr = Vec::new();
                let mut resize_scratch = LinearResizeScratch::default();
                write_resize_norm_img_into_slice_with_scratch(
                    image,
                    max_wh_ratio,
                    self.config.rec_img_shape,
                    &mut self.batch_scratch[..sample_len],
                    &mut tmp_bgr,
                    &mut resize_scratch,
                )?;
            }
            preprocess_ms += preprocess_start.elapsed().as_secs_f32() * 1000.0;

            let batch_view = ArrayView4::from_shape(
                (batch_size, img_channel, img_height, dst_width),
                &self.batch_scratch[..total_len],
            )
            .map_err(|e| {
                RapidOcrError::InvalidInput(format!("invalid rec batch tensor shape: {e}"))
            })?;
            let decoder = &self.decoder;
            let infer_start = Instant::now();
            let mut decode_ms = 0.0_f32;
            let (line_results, word_results) =
                self.session.run_array3_view_with(batch_view, |preds| {
                    let decode_start = Instant::now();
                    let result = decoder.decode_view(
                        preds,
                        options.return_word_box,
                        &wh_ratio_list,
                        max_wh_ratio as f32,
                    );
                    decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
                    result
                })?;
            let infer_total = infer_start.elapsed().as_secs_f32() * 1000.0;
            infer_ms += (infer_total - decode_ms).max(0.0);
            postprocess_ms += decode_ms;

            for (rno, (text, score)) in line_results.into_iter().enumerate() {
                let word_info = if options.return_word_box {
                    word_results.get(rno).cloned()
                } else {
                    None
                };

                let target_idx = indices[beg + rno];
                rec_res[target_idx] = Some(LineResult {
                    text,
                    score,
                    word_info,
                });
            }
        }

        let mut lines = Vec::with_capacity(images.len());
        for line in rec_res.into_iter().flatten() {
            lines.push(line);
        }

        if self.config.model.lang == LangRec::Arabic {
            for line in &mut lines {
                line.text = reorder_bidi_for_display(&line.text);
            }
        }

        Ok(RecognizeOutput {
            lines,
            elapsed: start.elapsed(),
            preprocess_ms,
            infer_ms,
            postprocess_ms,
        })
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.session.provider_resolution()
    }
}

fn resolve_model_path(
    config: &RecognizerConfig,
    resolved: &ResolvedRecModel,
    model_store_dir: &Path,
) -> Result<PathBuf> {
    if let Some(model_path) = &config.model.model_path {
        return verify_existing_file(model_path);
    }

    if !config.model.allow_download {
        return Err(RapidOcrError::Config(
            "model_path is not set and allow_download=false".to_string(),
        ));
    }

    // §6.4：哈希必填（默认表的 rec 条目都记录了 SHA-256）。
    let expected = require_model_hash(resolved.sha256.as_deref(), &resolved.model_url)?;
    download_verified(&DownloadRequest::new(
        &resolved.model_url,
        expected,
        model_store_dir,
    ))
}

fn resolve_character_path(
    config: &RecognizerConfig,
    resolved: &ResolvedRecModel,
    model_store_dir: &Path,
) -> Result<Option<PathBuf>> {
    if let Some(path) = &config.model.rec_keys_path {
        return Ok(Some(verify_existing_file(path)?));
    }

    let Some(dictionary) = &resolved.dictionary else {
        return Ok(None);
    };

    if !config.model.allow_download {
        return Err(RapidOcrError::Config(
            "character metadata missing and dict download disabled".to_string(),
        ));
    }

    // 字典和权重一样必须校验哈希：默认表里每个字典都记录了 SHA-256，
    // 因此这里不再传 `None`（"可传 None 的下载入口"本身就是 §1.2 记录的缺口，
    // 已在 §6.4 删除）。
    let expected = require_model_hash(dictionary.sha256.as_deref(), &dictionary.url)?;
    let path = download_verified(&DownloadRequest::new(
        &dictionary.url,
        expected,
        model_store_dir,
    ))?;
    Ok(Some(path))
}

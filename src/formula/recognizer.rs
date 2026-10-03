//! 独立公式识别 API。
//!
//! `FormulaRecognizer` 组合 typed formula session、公式预处理和 tokenizer，
//! 不扩展普通 CTC `Recognizer`，也不依赖 `LineResult`。

use std::path::Path;
use std::time::{Duration, Instant};

use image::DynamicImage;
use ndarray::Axis;
use serde::{Deserialize, Serialize};

use crate::{
    config::RuntimeConfig,
    error::{RapidOcrError, Result},
    formula::{
        preprocess::FormulaPreprocessor,
        session::FormulaSession,
        tokenizer::{FormulaDecode, FormulaTokenizer},
        tokenizer_metadata::FormulaTokenizerMetadata,
    },
    input::image_loader::{LoadImage, OcrInput},
    model_store::sha256_file,
    runtime::provider::ProviderResolution,
};

pub const DEFAULT_MAX_FORMULA_BATCH_SIZE: usize = 16;
pub const DEFAULT_MAX_FORMULA_INPUT_PIXELS: u64 = 24_000_000;
/// 公式输出序列长度上限。
///
/// 模型图内 `Loop` 自己的 trip count 决定了输出张量的最大宽度：当 batch 中任一
/// 样本在 Loop 预算内没有产生 EOS 时，ONNX Runtime 会把**整个 batch** 补齐到该宽度。
/// 实测 `pp_formulanet_plus_m.onnx` 的该宽度为 2561（见
/// `formula::recognizer::tests::default_sequence_limit_exceeds_model_loop_bound`）。
///
/// 因此上限必须**严格大于**模型的 Loop 宽度，否则一个不收敛的样本会导致整批被拒绝，
/// 连带丢掉同批中识别正确的样本。4096 既覆盖实测宽度，又仍然是明确的内存/解码上界。
pub const DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH: usize = 4096;

/// 实测的模型图内 `Loop` 输出宽度（`[N, 2561]`）。
pub const FORMULA_MODEL_LOOP_BOUND: usize = 2561;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaRecognition {
    pub latex: String,
    pub token_ids: Vec<i64>,
    pub eos_index: Option<usize>,
    pub truncated: bool,
    pub model_id: String,
    /// 产生该结果的**调用**墙钟耗时（毫秒）。
    ///
    /// 同一次 [`FormulaRecognizer::recognize_batch`] 返回的所有结果共享同一个值，
    /// 它是整个调用（**包含内部所有分块**）的端到端耗时，**不是**单样本耗时，
    /// 也不是单个分块的耗时；需要单样本估计时除以
    /// [`FormulaRecognition::batch_size`]。单图 [`FormulaRecognizer::recognize`]
    /// 的调用耗时等于单样本耗时。
    pub elapsed_ms: f32,
    /// 产生该结果的调用包含的图片数量（单图为 1；批量调用为整次调用的图片数，
    /// 不是某个分块的大小）。
    pub batch_size: usize,
}

impl FormulaRecognition {
    pub fn decode(&self) -> FormulaDecode {
        FormulaDecode {
            latex: self.latex.clone(),
            token_ids: self.token_ids.clone(),
            eos_index: self.eos_index,
            truncated: self.truncated,
        }
    }
}

#[derive(Debug)]
pub struct FormulaRecognizer {
    session: FormulaSession,
    preprocessor: FormulaPreprocessor,
    tokenizer: FormulaTokenizer,
    model_id: String,
    max_batch_size: usize,
    max_input_pixels: u64,
    max_sequence_length: usize,
}

impl FormulaRecognizer {
    pub fn from_model(model_path: &Path, runtime_cfg: &RuntimeConfig) -> Result<Self> {
        Self::from_model_with_hash(model_path, runtime_cfg, None)
    }

    pub fn from_model_with_hash(
        model_path: &Path,
        runtime_cfg: &RuntimeConfig,
        expected_sha256: Option<&str>,
    ) -> Result<Self> {
        if !model_path.is_file() {
            return Err(RapidOcrError::FileNotFound(model_path.to_path_buf()));
        }
        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(model_path)?;
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(RapidOcrError::HashMismatch {
                    path: model_path.to_path_buf(),
                    expected: expected.to_string(),
                    actual,
                });
            }
        }

        let session = FormulaSession::new(model_path, runtime_cfg)?;
        let character_metadata = session.character_metadata()?.ok_or_else(|| {
            RapidOcrError::Tokenizer(format!(
                "formula model is missing `character` metadata: {}",
                model_path.display()
            ))
        })?;
        let metadata = FormulaTokenizerMetadata::from_character_metadata(&character_metadata)?;
        let tokenizer = FormulaTokenizer::from_metadata(&metadata)?;

        let model_id = model_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("formula-model")
            .to_string();

        Ok(Self {
            session,
            preprocessor: FormulaPreprocessor::new(),
            tokenizer,
            model_id,
            max_batch_size: DEFAULT_MAX_FORMULA_BATCH_SIZE,
            max_input_pixels: DEFAULT_MAX_FORMULA_INPUT_PIXELS,
            max_sequence_length: DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }

    pub fn set_max_batch_size(&mut self, max_batch_size: usize) -> Result<()> {
        if max_batch_size == 0 {
            return Err(RapidOcrError::InvalidInput(
                "formula max batch size must be greater than zero".to_string(),
            ));
        }
        self.max_batch_size = max_batch_size;
        Ok(())
    }

    pub fn max_input_pixels(&self) -> u64 {
        self.max_input_pixels
    }

    pub fn set_max_input_pixels(&mut self, max_input_pixels: u64) -> Result<()> {
        if max_input_pixels == 0 {
            return Err(RapidOcrError::InvalidInput(
                "formula max input pixels must be greater than zero".to_string(),
            ));
        }
        self.max_input_pixels = max_input_pixels;
        Ok(())
    }

    pub fn max_sequence_length(&self) -> usize {
        self.max_sequence_length
    }

    pub fn set_max_sequence_length(&mut self, max_sequence_length: usize) -> Result<()> {
        if max_sequence_length == 0 {
            return Err(RapidOcrError::InvalidInput(
                "formula max sequence length must be greater than zero".to_string(),
            ));
        }
        self.max_sequence_length = max_sequence_length;
        Ok(())
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.session.provider_resolution()
    }

    /// 该识别器会话下发给 ONNX Runtime 的 `(intra, inter)` 线程数。
    ///
    /// 值来自 [`RuntimeConfig::effective_session_threads`]，与引擎路径
    /// （[`crate::runtime::profile::RuntimeProfile::plan`]）是同一个函数；
    /// `None` 表示该线程数**未配置**（ORT 用自己的默认值），不是 0 线程。
    pub fn session_threads(&self) -> (Option<usize>, Option<usize>) {
        self.session.session_threads()
    }

    pub fn recognize(&mut self, image: &DynamicImage) -> Result<FormulaRecognition> {
        self.ensure_image_limit(image)?;
        let start = Instant::now();
        let input = self.preprocessor.preprocess(image)?;
        let output = self.session.run(input.view())?;
        if output.nrows() != 1 {
            return Err(RapidOcrError::Decode(format!(
                "formula model returned {} rows for single input (model={})",
                output.nrows(),
                self.model_id
            )));
        }
        self.ensure_sequence_limit(output.ncols())?;
        let token_ids = output.row(0).to_vec();
        let decoded = self.tokenizer.decode_ids(&token_ids)?;
        Ok(self.recognition(decoded, start.elapsed().as_secs_f32() * 1000.0, 1))
    }

    /// Recognizes a formula from an in-memory encoded image.
    ///
    /// Encoded-byte, decoded-pixel and header-probe limits are enforced by the
    /// shared input layer (`LoadImage`), so formula and ordinary OCR cannot
    /// drift apart in their input semantics.
    pub fn recognize_encoded(
        &mut self,
        bytes: &[u8],
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<FormulaRecognition> {
        let image = LoadImage::default().load_dynamic_with_limit(
            OcrInput::Bytes(bytes.to_vec()),
            max_decode_pixels,
            max_encoded_bytes,
        )?;
        self.recognize(&image)
    }

    pub fn recognize_file(
        &mut self,
        path: &Path,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<FormulaRecognition> {
        let image = LoadImage::default().load_dynamic_with_limit(
            OcrInput::Path(path.to_path_buf()),
            max_decode_pixels,
            max_encoded_bytes,
        )?;
        self.recognize(&image)
    }

    pub fn recognize_url(
        &mut self,
        url: &str,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
        timeout: Duration,
    ) -> Result<FormulaRecognition> {
        let loader = LoadImage::with_http_timeouts(timeout, timeout);
        let image = loader.load_dynamic_with_limit(
            OcrInput::Url(url.to_string()),
            max_decode_pixels,
            max_encoded_bytes,
        )?;
        self.recognize(&image)
    }

    /// 批量识别公式，**按 `max_batch_size` 自动分块**，结果顺序与输入一致。
    ///
    /// 调用方不需要知道批大小上限：一页可能有几十个公式区域
    /// （`FormulaPolicy::max_regions` 默认 64），而模型批大小默认只有 16。
    /// 过去的实现把整页裁剪一次性送进来，超过上限就整页报错，于是 17–64 个公式区域
    /// 的页面会**整体失败**；分块让“批大小上限”只影响单次推理的形状，不影响正确性。
    ///
    /// 语义（与单块实现一致）：
    ///
    /// - 返回值的 `elapsed_ms` 是**整个调用**的墙钟耗时（包含所有分块），
    ///   `batch_size` 是**整个调用**的图片数量；两者对所有结果相同。
    pub fn recognize_batch(&mut self, images: &[DynamicImage]) -> Result<Vec<FormulaRecognition>> {
        if images.is_empty() {
            return Ok(Vec::new());
        }

        for image in images {
            self.ensure_image_limit(image)?;
        }

        let start = Instant::now();
        let mut results = Vec::with_capacity(images.len());
        for chunk in images.chunks(self.max_batch_size) {
            results.extend(self.recognize_chunk(chunk)?);
        }
        let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
        for result in &mut results {
            result.elapsed_ms = elapsed_ms;
            result.batch_size = images.len();
        }
        Ok(results)
    }

    /// 单个分块的推理与解码；`images.len()` 必须 <= `self.max_batch_size`。
    fn recognize_chunk(&mut self, images: &[DynamicImage]) -> Result<Vec<FormulaRecognition>> {
        let input = self.preprocessor.preprocess_batch(images)?;
        let output = self.session.run(input.view())?;
        if output.nrows() != images.len() {
            return Err(RapidOcrError::Decode(format!(
                "formula model returned {} rows for batch size {} (model={})",
                output.nrows(),
                images.len(),
                self.model_id
            )));
        }
        self.ensure_sequence_limit(output.ncols())?;

        let mut results = Vec::with_capacity(images.len());
        for row in output.axis_iter(Axis(0)) {
            let token_ids = row.to_vec();
            let decoded = self.tokenizer.decode_ids(&token_ids)?;
            results.push(self.recognition(decoded, 0.0, images.len()));
        }
        Ok(results)
    }

    fn ensure_image_limit(&self, image: &DynamicImage) -> Result<()> {
        use image::GenericImageView;
        let (width, height) = image.dimensions();
        crate::input::image_loader::ensure_decode_pixels(
            width as usize,
            height as usize,
            self.max_input_pixels,
        )
    }

    fn ensure_sequence_limit(&self, sequence_length: usize) -> Result<()> {
        if sequence_length > self.max_sequence_length {
            return Err(RapidOcrError::Decode(format!(
                "formula output length {sequence_length} exceeds limit {}",
                self.max_sequence_length
            )));
        }
        Ok(())
    }
    fn recognition(
        &self,
        decoded: FormulaDecode,
        elapsed_ms: f32,
        batch_size: usize,
    ) -> FormulaRecognition {
        FormulaRecognition {
            latex: crate::formula::postprocess::postprocess_latex(&decoded.latex),
            token_ids: decoded.token_ids,
            eos_index: decoded.eos_index,
            truncated: decoded.truncated,
            model_id: self.model_id.clone(),
            elapsed_ms,
            batch_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/formula-onnx")
            .join(name)
    }

    fn rt() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    fn gray(value: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 32, Rgb([value, value, value])))
    }

    #[test]
    fn single_image_happy_path() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        let result = recognizer.recognize(&gray(255)).expect("recognize");
        assert_eq!(result.latex, "\\cdot");
        assert_eq!(result.token_ids, vec![0, 82, 1769, 2]);
        assert_eq!(result.eos_index, Some(3));
        assert!(!result.truncated);
        assert_eq!(result.model_id, "formula_recognizer_ok");
    }

    #[test]
    fn batch_preserves_order_and_independence() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        let images = vec![gray(0), gray(255), gray(128), gray(64)];
        let results = recognizer
            .recognize_batch(&images)
            .expect("batch recognize");
        assert_eq!(results.len(), images.len());
        for result in &results {
            assert_eq!(result.latex, "\\cdot");
            assert_eq!(result.token_ids, vec![0, 82, 1769, 2]);
            assert_eq!(result.eos_index, Some(3));
            assert!(!result.truncated);
        }
    }

    #[test]
    fn empty_batch_is_supported() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        assert!(
            recognizer
                .recognize_batch(&[])
                .expect("empty batch")
                .is_empty()
        );
    }

    /// 超过 `max_batch_size` 的批次必须**内部分块**，而不是整批失败。
    ///
    /// 根因：`FormulaPolicy::max_regions` 默认 64，而模型批大小默认 16，过去的实现
    /// 把整页裁剪一次性送进来并在超限时返回错误，于是 17–64 个公式区域的页面会整体失败。
    #[test]
    fn batches_larger_than_max_batch_size_are_chunked() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        recognizer.set_max_batch_size(1).expect("set batch size");
        let images = vec![gray(0), gray(255), gray(128)];
        let results = recognizer
            .recognize_batch(&images)
            .expect("a batch larger than max_batch_size must be chunked, not rejected");
        assert_eq!(results.len(), images.len());
        for result in &results {
            assert_eq!(result.latex, "\\cdot");
            assert_eq!(result.token_ids, vec![0, 82, 1769, 2]);
            assert_eq!(
                result.batch_size, 3,
                "batch_size must describe the whole call, not one chunk"
            );
        }
        assert!(
            (results[0].elapsed_ms - results[2].elapsed_ms).abs() < f32::EPSILON,
            "every result of one call shares the call wall time"
        );
    }

    /// 一页 20 个公式区域（> 默认批大小 16）必须返回 20 个结果，且与显式分成 16 + 4
    /// 两次调用逐项一致。
    ///
    /// 顺序说明：`formula_recognizer_ok.onnx` 的每一行都是固定 token（只有 batch 维
    /// 依赖输入），因此内容上无法区分“第 1 个”和“第 20 个”。这里能断言的是数量、
    /// 每项内容与“分块对结果透明”这一不变量；分块顺序由 `results.extend(chunk)` 与
    /// `axis_iter` 的行序结构性保证。
    #[test]
    fn twenty_crops_are_chunked_and_match_explicit_chunks() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        assert_eq!(recognizer.max_batch_size(), DEFAULT_MAX_FORMULA_BATCH_SIZE);
        const _: () = assert!(
            20 > DEFAULT_MAX_FORMULA_BATCH_SIZE,
            "this test must exercise more than one chunk"
        );

        let images: Vec<DynamicImage> = (0..20).map(|i| gray((i * 11) as u8)).collect();
        let results = recognizer
            .recognize_batch(&images)
            .expect("20 crops must be recognized across chunks");
        assert_eq!(results.len(), 20);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result.latex, "\\cdot", "crop {index}");
            assert_eq!(result.token_ids, vec![0, 82, 1769, 2], "crop {index}");
            assert_eq!(result.eos_index, Some(3), "crop {index}");
            assert_eq!(result.batch_size, 20, "crop {index}");
        }
        let wall = results[0].elapsed_ms;
        assert!(
            results
                .iter()
                .all(|r| (r.elapsed_ms - wall).abs() < f32::EPSILON),
            "the whole call's wall time is shared by all results"
        );

        let explicit: Vec<_> = recognizer
            .recognize_batch(&images[..16])
            .expect("first explicit chunk")
            .into_iter()
            .chain(
                recognizer
                    .recognize_batch(&images[16..])
                    .expect("second explicit chunk"),
            )
            .collect();
        assert_eq!(explicit.len(), results.len());
        for (chunked, split) in results.iter().zip(&explicit) {
            assert_eq!(chunked.latex, split.latex);
            assert_eq!(chunked.token_ids, split.token_ids);
            assert_eq!(chunked.eos_index, split.eos_index);
            assert_eq!(chunked.model_id, split.model_id);
        }
    }

    /// 显式把批大小设成 1 时，内部仍然必须逐块处理 N 张图，而不是只处理第一块。
    #[test]
    fn single_image_chunking_covers_every_input() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        recognizer.set_max_batch_size(1).expect("set batch size");
        let images: Vec<DynamicImage> = (0..5).map(|i| gray((i * 40) as u8)).collect();
        let results = recognizer
            .recognize_batch(&images)
            .expect("chunked batch recognize");
        assert_eq!(results.len(), 5);
        for result in &results {
            assert_eq!(result.batch_size, 5);
            assert_eq!(result.latex, "\\cdot");
        }
    }

    #[test]
    fn missing_model_is_located() {
        let error = FormulaRecognizer::from_model(&fixture("does_not_exist.onnx"), &rt())
            .expect_err("missing model must fail");
        assert!(matches!(error, RapidOcrError::FileNotFound(_)));
    }

    #[test]
    fn hash_mismatch_is_rejected() {
        let expected = "0000000000000000000000000000000000000000000000000000000000000000";
        let error = FormulaRecognizer::from_model_with_hash(
            &fixture("formula_recognizer_ok.onnx"),
            &rt(),
            Some(expected),
        )
        .expect_err("hash mismatch must fail");
        assert!(matches!(error, RapidOcrError::HashMismatch { .. }));
    }

    #[test]
    fn corrupted_tokenizer_metadata_is_rejected() {
        let error = FormulaRecognizer::from_model(&fixture("formula_bad_metadata.onnx"), &rt())
            .expect_err("bad metadata must fail");
        assert!(
            error.to_string().contains("metadata") || error.to_string().contains("tokenizer"),
            "error: {error}"
        );
    }

    #[test]
    fn out_of_vocab_model_output_is_rejected() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_bad_token.onnx"), &rt())
                .expect("recognizer should load");
        let error = recognizer
            .recognize(&gray(255))
            .expect_err("bad token must fail");
        assert!(
            error.to_string().contains("out of vocabulary"),
            "error: {error}"
        );
    }

    #[test]
    fn empty_image_is_rejected_before_inference() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        let empty = DynamicImage::ImageRgb8(RgbImage::from_raw(0, 0, Vec::new()).unwrap());
        let error = recognizer
            .recognize(&empty)
            .expect_err("empty image must fail");
        assert!(
            error.to_string().contains("greater than zero"),
            "error: {error}"
        );
    }

    /// 默认序列上限必须严格大于模型图内 Loop 的输出宽度。
    ///
    /// 否则一个不收敛的样本会把整个 batch 的输出张量补齐到 Loop 宽度，从而让
    /// `ensure_sequence_limit` 拒绝整批，连带丢掉同批中识别正确的样本。
    /// 实测宽度来自真实模型在 501 张 val 上的输出（不收敛时的张量宽度为 2561）。
    #[test]
    fn default_sequence_limit_exceeds_model_loop_bound() {
        const _: () = assert!(DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH > FORMULA_MODEL_LOOP_BOUND);
        let recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        assert_eq!(
            recognizer.max_sequence_length(),
            DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH
        );
        assert!(
            recognizer
                .ensure_sequence_limit(FORMULA_MODEL_LOOP_BOUND)
                .is_ok(),
            "a row at the model's own loop bound must be accepted"
        );
        assert!(
            recognizer
                .ensure_sequence_limit(DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH + 1)
                .is_err(),
            "the limit must still bound memory for pathological outputs"
        );
    }

    #[test]
    fn pixel_and_sequence_limits_are_enforced() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        recognizer
            .set_max_input_pixels(100)
            .expect("set pixel limit");
        let error = recognizer
            .recognize(&gray(255))
            .expect_err("large input must fail");
        assert!(error.to_string().contains("limit is 100"), "error: {error}");

        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        recognizer
            .set_max_sequence_length(2)
            .expect("set sequence limit");
        let error = recognizer
            .recognize(&gray(255))
            .expect_err("long output must fail");
        assert!(
            error.to_string().contains("exceeds limit"),
            "error: {error}"
        );
    }

    #[test]
    fn encoded_input_enforces_encoded_and_pixel_limits() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        let encoded = {
            let image = gray(255).to_rgb8();
            let mut bytes = Vec::new();
            image
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .expect("encode png");
            bytes
        };
        let result = recognizer
            .recognize_encoded(&encoded, 2000, 10_000)
            .expect("encoded recognize");
        assert_eq!(result.latex, "\\cdot");

        let error = recognizer
            .recognize_encoded(&encoded, 2000, 10)
            .expect_err("encoded limit must fail");
        assert!(error.to_string().contains("limit is 10"), "error: {error}");

        let error = recognizer
            .recognize_encoded(&encoded, 100, 10_000)
            .expect_err("pixel limit must fail");
        assert!(error.to_string().contains("limit is 100"), "error: {error}");
    }

    /// 文件输入必须与共享输入层使用同一套限制语义与错误类型。
    #[test]
    fn file_input_uses_shared_loader_limits() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");

        let error = recognizer
            .recognize_file(Path::new("does-not-exist.png"), 10_000, 10_000)
            .expect_err("missing file must be reported as FileNotFound");
        assert!(
            matches!(error, RapidOcrError::FileNotFound(_)),
            "error: {error}"
        );

        let directory = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-formula-loader-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("formula.png");
        let image = gray(255).to_rgb8();
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode png");
        std::fs::write(&path, &bytes).expect("write png");

        let result = recognizer
            .recognize_file(&path, 2000, 10_000)
            .expect("file recognize");
        assert_eq!(result.latex, "\\cdot");

        let error = recognizer
            .recognize_file(&path, 2000, 10)
            .expect_err("encoded file limit must fail");
        assert!(
            error.to_string().contains("encoded image has")
                && error.to_string().contains("limit is 10"),
            "error: {error}"
        );

        let error = recognizer
            .recognize_file(&path, 100, 10_000)
            .expect_err("pixel file limit must fail");
        assert!(error.to_string().contains("limit is 100"), "error: {error}");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// 公式域严格 provider 语义：请求不可用的加速器必须失败，不能静默用 CPU。
    #[test]
    #[cfg(not(feature = "cuda-provider"))]
    fn unavailable_provider_is_rejected_not_silently_fallen_back() {
        let runtime = RuntimeConfig {
            provider_preference: crate::config::ProviderPreference::Cuda { device_id: 0 },
            fail_if_provider_unavailable: false,
            ..RuntimeConfig::default()
        };
        let error = FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &runtime)
            .expect_err("unavailable accelerator must fail");
        assert!(
            matches!(error, RapidOcrError::UnsupportedProvider(_)),
            "error: {error}"
        );
    }

    /// batch 结果的耗时语义必须明确：同一次调用的所有结果共享调用总耗时。
    #[test]
    fn batch_results_share_call_wall_time() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        let images = vec![gray(0), gray(255), gray(128)];
        let results = recognizer
            .recognize_batch(&images)
            .expect("batch recognize");
        assert_eq!(results.len(), 3);
        for result in &results {
            assert_eq!(result.batch_size, 3);
            assert!(
                (result.elapsed_ms - results[0].elapsed_ms).abs() < f32::EPSILON,
                "batch results must share the same call wall time"
            );
            assert!(result.elapsed_ms >= 0.0);
        }

        let single = recognizer.recognize(&gray(255)).expect("single recognize");
        assert_eq!(single.batch_size, 1);
    }
}

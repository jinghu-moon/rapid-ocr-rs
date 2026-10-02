//! 独立公式识别 API。
//!
//! `FormulaRecognizer` 组合 typed formula session、公式预处理和 tokenizer，
//! 不扩展普通 CTC `Recognizer`，也不依赖 `LineResult`。

use std::io::{Cursor, Read};
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
    model_store::sha256_file,
    runtime::provider::ProviderResolution,
};

pub const DEFAULT_MAX_FORMULA_BATCH_SIZE: usize = 16;
pub const DEFAULT_MAX_FORMULA_INPUT_PIXELS: u64 = 24_000_000;
pub const DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH: usize = 2560;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaRecognition {
    pub latex: String,
    pub token_ids: Vec<i64>,
    pub eos_index: Option<usize>,
    pub truncated: bool,
    pub model_id: String,
    pub elapsed_ms: f32,
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
        Ok(self.recognition(decoded, start.elapsed().as_secs_f32() * 1000.0))
    }

    pub fn recognize_encoded(
        &mut self,
        bytes: &[u8],
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<FormulaRecognition> {
        if bytes.len() as u64 > max_encoded_bytes {
            return Err(RapidOcrError::InvalidImage(format!(
                "encoded formula image has {} bytes, limit is {max_encoded_bytes}",
                bytes.len()
            )));
        }
        let dimensions = image_dimensions_from_bytes(bytes)?;
        ensure_pixel_limit(dimensions.0, dimensions.1, max_decode_pixels)?;
        let image = image::load_from_memory(bytes)
            .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))?;
        self.recognize(&image)
    }

    pub fn recognize_file(
        &mut self,
        path: &Path,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<FormulaRecognition> {
        if !path.is_file() {
            return Err(RapidOcrError::FileNotFound(path.to_path_buf()));
        }
        let encoded_len = std::fs::metadata(path)?.len();
        if encoded_len > max_encoded_bytes {
            return Err(RapidOcrError::InvalidImage(format!(
                "encoded formula image has {encoded_len} bytes, limit is {max_encoded_bytes}"
            )));
        }
        let dimensions = image_dimensions_from_file(path)?;
        ensure_pixel_limit(dimensions.0, dimensions.1, max_decode_pixels)?;
        let image =
            image::open(path).map_err(|error| RapidOcrError::InvalidImage(error.to_string()))?;
        self.recognize(&image)
    }

    pub fn recognize_url(
        &mut self,
        url: &str,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
        timeout: Duration,
    ) -> Result<FormulaRecognition> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()?;
        let response = client.get(url).send()?;
        if !response.status().is_success() {
            return Err(RapidOcrError::Download(format!(
                "failed to fetch formula image from url {url}: HTTP {}",
                response.status()
            )));
        }
        if let Some(content_length) = response.content_length() {
            ensure_encoded_limit(content_length, max_encoded_bytes)?;
        }
        let read_limit = max_encoded_bytes.saturating_add(1);
        let mut bytes = Vec::new();
        response.take(read_limit).read_to_end(&mut bytes)?;
        ensure_encoded_limit(bytes.len() as u64, max_encoded_bytes)?;
        self.recognize_encoded(&bytes, max_decode_pixels, max_encoded_bytes)
    }

    pub fn recognize_batch(&mut self, images: &[DynamicImage]) -> Result<Vec<FormulaRecognition>> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        if images.len() > self.max_batch_size {
            return Err(RapidOcrError::InvalidInput(format!(
                "formula batch size {} exceeds limit {}",
                images.len(),
                self.max_batch_size
            )));
        }

        for image in images {
            self.ensure_image_limit(image)?;
        }

        let start = Instant::now();
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
            results.push(self.recognition(decoded, start.elapsed().as_secs_f32() * 1000.0));
        }
        Ok(results)
    }

    fn ensure_image_limit(&self, image: &DynamicImage) -> Result<()> {
        use image::GenericImageView;
        let (width, height) = image.dimensions();
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.max_input_pixels {
            return Err(RapidOcrError::InvalidImage(format!(
                "formula image has {pixels} pixels, limit is {}",
                self.max_input_pixels
            )));
        }
        Ok(())
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

    fn recognition(&self, decoded: FormulaDecode, elapsed_ms: f32) -> FormulaRecognition {
        FormulaRecognition {
            latex: crate::formula::postprocess::postprocess_latex(&decoded.latex),
            token_ids: decoded.token_ids,
            eos_index: decoded.eos_index,
            truncated: decoded.truncated,
            model_id: self.model_id.clone(),
            elapsed_ms,
        }
    }
}

fn image_dimensions_from_bytes(bytes: &[u8]) -> Result<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))?
        .into_dimensions()
        .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))
}

fn image_dimensions_from_file(path: &Path) -> Result<(u32, u32)> {
    image::ImageReader::open(path)
        .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))?
        .with_guessed_format()
        .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))?
        .into_dimensions()
        .map_err(|error| RapidOcrError::InvalidImage(error.to_string()))
}

fn ensure_encoded_limit(actual: u64, max_encoded_bytes: u64) -> Result<()> {
    if actual > max_encoded_bytes {
        return Err(RapidOcrError::InvalidImage(format!(
            "encoded formula image has {actual} bytes, limit is {max_encoded_bytes}"
        )));
    }
    Ok(())
}

fn ensure_pixel_limit(width: u32, height: u32, max_decode_pixels: u64) -> Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > max_decode_pixels {
        return Err(RapidOcrError::InvalidImage(format!(
            "formula image has {pixels} pixels, limit is {max_decode_pixels}"
        )));
    }
    Ok(())
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

    #[test]
    fn oversized_batch_is_rejected() {
        let mut recognizer =
            FormulaRecognizer::from_model(&fixture("formula_recognizer_ok.onnx"), &rt())
                .expect("recognizer should load");
        recognizer.set_max_batch_size(1).expect("set batch size");
        let error = recognizer
            .recognize_batch(&[gray(0), gray(255)])
            .expect_err("oversized batch must fail");
        assert!(
            error.to_string().contains("exceeds limit"),
            "error: {error}"
        );
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
}

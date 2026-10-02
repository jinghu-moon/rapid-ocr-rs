//! 独立公式识别 API。
//!
//! `FormulaRecognizer` 组合 typed formula session、公式预处理和 tokenizer，
//! 不扩展普通 CTC `Recognizer`，也不依赖 `LineResult`。

use std::path::Path;
use std::time::Instant;

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

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.session.provider_resolution()
    }

    pub fn recognize(&mut self, image: &DynamicImage) -> Result<FormulaRecognition> {
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
        let token_ids = output.row(0).to_vec();
        let decoded = self.tokenizer.decode_ids(&token_ids)?;
        Ok(self.recognition(decoded, start.elapsed().as_secs_f32() * 1000.0))
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

        let mut results = Vec::with_capacity(images.len());
        for row in output.axis_iter(Axis(0)) {
            let token_ids = row.to_vec();
            let decoded = self.tokenizer.decode_ids(&token_ids)?;
            results.push(self.recognition(decoded, start.elapsed().as_secs_f32() * 1000.0));
        }
        Ok(results)
    }

    fn recognition(&self, decoded: FormulaDecode, elapsed_ms: f32) -> FormulaRecognition {
        FormulaRecognition {
            latex: decoded.latex,
            token_ids: decoded.token_ids,
            eos_index: decoded.eos_index,
            truncated: decoded.truncated,
            model_id: self.model_id.clone(),
            elapsed_ms,
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
}

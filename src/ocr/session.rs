//! 普通 OCR typed session 契约。
//!
//! 共享 `runtime::session::OrtSession` 只负责打开模型；本模块负责普通 OCR
//! 的 FLOAT rank-4 输入和领域输出 rank/dtype/metadata 校验。

use std::path::Path;

use ndarray::{ArrayView2, ArrayView3, ArrayView4};
use ort::value::TensorElementType;

use crate::{
    config::RuntimeConfig,
    error::Result,
    runtime::{
        contracts::{require_single_input, require_single_output, require_tensor},
        provider::ProviderResolution,
        session::OrtSession,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrSessionKind {
    Rec,
    Cls,
    Det,
}

impl OcrSessionKind {
    fn output_rank(self) -> usize {
        match self {
            Self::Rec => 3,
            Self::Cls => 2,
            Self::Det => 4,
        }
    }
}

#[derive(Debug)]
pub struct OcrSession {
    inner: OrtSession,
    character_list: Option<Vec<String>>,
}

impl OcrSession {
    pub fn new(
        model_path: &Path,
        runtime_cfg: &RuntimeConfig,
        kind: OcrSessionKind,
    ) -> Result<Self> {
        let inner = OrtSession::open_unchecked(model_path, runtime_cfg)?;
        let probe = inner.probe_io()?;
        let input = require_single_input(&probe, model_path, "OCR")?;
        require_tensor(input, "input", 4, TensorElementType::Float32, model_path)?;
        let output = require_single_output(&probe, model_path, "OCR")?;
        require_tensor(
            output,
            "output",
            kind.output_rank(),
            TensorElementType::Float32,
            model_path,
        )?;

        let character_list = if kind == OcrSessionKind::Rec {
            inner
                .metadata_custom("character")?
                .map(|raw| raw.lines().map(|line| line.to_string()).collect::<Vec<_>>())
        } else {
            None
        };

        Ok(Self {
            inner,
            character_list,
        })
    }

    pub fn provider_resolution(&self) -> ProviderResolution {
        self.inner.provider_resolution()
    }

    pub fn take_character_list(&mut self) -> Option<Vec<String>> {
        self.character_list.take()
    }

    pub fn run_array2_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView2<'a, f32>) -> Result<T>,
    {
        self.inner.run_array2_view_with(input, f)
    }

    pub fn run_array3_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ArrayView3<'a, f32>) -> Result<T>,
    {
        self.inner.run_array3_view_with(input, f)
    }

    pub fn run_array4_view_with<T, F>(&mut self, input: ArrayView4<'_, f32>, f: F) -> Result<T>
    where
        F: for<'a> FnOnce(ndarray::ArrayView4<'a, f32>) -> Result<T>,
    {
        self.inner.run_array4_view_with(input, f)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ndarray::Array4;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ocr-onnx")
            .join(name)
    }

    fn rt() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    #[test]
    fn rec_session_accepts_ctc_contract_and_runs() {
        let mut session = OcrSession::new(&fixture("ocr_rec_ok.onnx"), &rt(), OcrSessionKind::Rec)
            .expect("ctc fixture should load");
        let input = Array4::<f32>::zeros((1, 3, 48, 320));
        let shape = session
            .run_array3_view_with(input.view(), |view| Ok(view.shape().to_vec()))
            .expect("ctc fixture should run");
        assert_eq!(shape, vec![1, 2, 3]);
    }

    #[test]
    fn cls_and_det_sessions_accept_matching_contracts() {
        let mut cls = OcrSession::new(&fixture("ocr_cls_ok.onnx"), &rt(), OcrSessionKind::Cls)
            .expect("cls fixture should load");
        let cls_input = Array4::<f32>::zeros((1, 3, 48, 192));
        let cls_shape = cls
            .run_array2_view_with(cls_input.view(), |view| Ok(view.shape().to_vec()))
            .expect("cls fixture should run");
        assert_eq!(cls_shape, vec![1, 2]);

        let mut det = OcrSession::new(&fixture("ocr_det_ok.onnx"), &rt(), OcrSessionKind::Det)
            .expect("det fixture should load");
        let det_input = Array4::<f32>::zeros((1, 3, 32, 32));
        let det_shape = det
            .run_array4_view_with(det_input.view(), |view| Ok(view.shape().to_vec()))
            .expect("det fixture should run");
        assert_eq!(det_shape, vec![1, 1, 1, 1]);
    }

    #[test]
    fn rec_session_rejects_formula_model_output_contract() {
        let error = OcrSession::new(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/formula-onnx/formula_ok.onnx"),
            &rt(),
            OcrSessionKind::Rec,
        )
        .expect_err("formula model must not load as ctc rec");
        assert!(error.to_string().contains("rank"), "error: {error}");
    }

    #[test]
    fn rec_session_rejects_bad_output_rank() {
        let error = OcrSession::new(
            &fixture("ocr_rec_output_rank2.onnx"),
            &rt(),
            OcrSessionKind::Rec,
        )
        .expect_err("rank2 rec output must fail");
        assert!(error.to_string().contains("rank"), "error: {error}");
    }

    #[test]
    fn rec_session_rejects_bad_output_dtype() {
        let error = OcrSession::new(
            &fixture("ocr_rec_output_int64.onnx"),
            &rt(),
            OcrSessionKind::Rec,
        )
        .expect_err("int64 rec output must fail");
        assert!(error.to_string().contains("FLOAT32"), "error: {error}");
    }

    #[test]
    fn rec_session_rejects_multi_input_and_multi_output() {
        let input_error = OcrSession::new(
            &fixture("ocr_rec_multi_input.onnx"),
            &rt(),
            OcrSessionKind::Rec,
        )
        .expect_err("multi input must fail");
        assert!(
            input_error.to_string().contains("exactly one input"),
            "error: {input_error}"
        );

        let output_error = OcrSession::new(
            &fixture("ocr_rec_multi_output.onnx"),
            &rt(),
            OcrSessionKind::Rec,
        )
        .expect_err("multi output must fail");
        assert!(
            output_error.to_string().contains("exactly one output"),
            "error: {output_error}"
        );
    }
}

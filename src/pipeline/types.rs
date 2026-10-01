use crate::{
    Quad,
    det::detector::DetTimingBreakdown,
    types::{LineResult, WordBox},
};

/// Internal execution snapshot. It never crosses the public crate boundary.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutionOutput {
    pub boxes: Option<Vec<Quad>>,
    pub det_scores: Option<Vec<f32>>,
    pub txts: Option<Vec<String>>,
    pub scores: Option<Vec<f32>>,
    pub word_boxes: Option<Vec<Vec<WordBox>>>,
    pub cls_res: Option<Vec<(String, f32)>>,
    pub lines: Option<Vec<LineResult>>,
    pub elapsed_ms: [Option<f32>; 3],
    pub e2e_ms: Option<f32>,
    pub det_breakdown_ms: Option<DetTimingBreakdown>,
    pub postprocess_ms: Option<f32>,
    pub decode_ms: Option<f32>,
    pub resize_ms: Option<f32>,
    pub crop_ms: Option<f32>,
    pub cls_breakdown_ms: Option<[f32; 3]>,
    pub rec_breakdown_ms: Option<[f32; 3]>,
    pub processed_size: Option<(u32, u32)>,
}
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutionOptions {
    pub use_det: bool,
    pub use_cls: bool,
    pub apply_cls_rotation: bool,
    pub use_rec: bool,
    pub return_word_box: bool,
    pub return_single_char_box: bool,
    pub text_score: Option<f32>,
    pub box_thresh: Option<f32>,
    pub unclip_ratio: Option<f32>,
}

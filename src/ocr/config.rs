use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{LangRec, ModelType, OcrVersion};

/// 普通 OCR 识别模型配置：模型文件、字典、版本与下载策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub lang: LangRec,
    pub ocr_version: OcrVersion,
    pub model_type: ModelType,
    pub model_path: Option<PathBuf>,
    pub rec_keys_path: Option<PathBuf>,
    pub allow_download: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            lang: LangRec::default(),
            ocr_version: OcrVersion::default(),
            model_type: ModelType::default(),
            model_path: None,
            rec_keys_path: None,
            allow_download: true,
        }
    }
}

/// 普通 OCR 识别器配置（rec 阶段）。
///
/// 这里没有 `runtime` 字段：线程/provider/arena 由 `EngineConfig::runtime` 唯一表达，
/// 见 `runtime::profile`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecognizerConfig {
    pub model: ModelConfig,
    pub rec_batch_num: usize,
    pub rec_img_shape: [usize; 3],
    pub model_store_dir: Option<PathBuf>,
}

impl Default for RecognizerConfig {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            rec_batch_num: 6,
            rec_img_shape: [3, 48, 320],
            model_store_dir: None,
        }
    }
}

/// 普通 OCR 词级/单字框输出开关。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RecognizeOptions {
    pub return_word_box: bool,
    pub return_single_char_box: bool,
}

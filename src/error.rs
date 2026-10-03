use std::path::PathBuf;

use thiserror::Error;

use crate::model_store::DownloadError;

#[derive(Debug, Error)]
pub enum RapidOcrError {
    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("model resolution failed: {0}")]
    ModelResolve(String),

    /// 下载失败。载荷是**分类**（`docs/05` §6.1 第 12 条的十二类），不是字符串：
    /// serve 侧靠它给出状态码/`code`/`kind`，因此同一个失败原因不可能有两种表示。
    #[error(transparent)]
    Download(#[from] DownloadError),

    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("invalid image: {0}")]
    InvalidImage(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("decoding failed: {0}")]
    Decode(String),

    #[error("tokenizer error: {0}")]
    Tokenizer(String),

    #[error("unsupported provider for v1: {0}")]
    UnsupportedProvider(String),

    #[error("unsupported runtime backend for v1: {0}")]
    UnsupportedBackend(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error("hash mismatch for {path:?}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

pub type Result<T> = std::result::Result<T, RapidOcrError>;

//! `ServeError`、状态码映射与 JSON 错误体（§11.1，另见 §6.1、§7.6、§9.5）。
//!
//! # 规则
//!
//! 1. **禁止字符串匹配**：HTTP 状态码与 `code` 只由这里的类型匹配产生；
//!    `RapidOcrError` → 状态码/code 的映射只有一处（[`classify_ocr_error`]）；
//! 2. 每个变体都必须有确定的状态码与 `code`，没有"默认 500"的兜底分支；
//! 3. 响应体形状固定为 `{ "code", "message", "detail" }`，三个键**始终**存在
//!    （没有额外信息时 `detail` 是 `null`，而不是缺键）。
//!
//! # 状态码与 `code` 全表
//!
//! | 变体 | 状态码 | `code` | 出处 |
//! | --- | --- | --- | --- |
//! | `BadRequest` | 400 | `bad_request` | §4.4（Content-Length 与实际不符、方法/路径外的问题） |
//! | `Unauthorized` | 401 | `unauthorized` | §11.1 |
//! | `DownloadsDisabled` | 403 | `downloads_disabled` | §11.1 |
//! | `BadOrigin` | 403 | `bad_origin` | §7.2 |
//! | `PayloadTooLarge` | 413 | `payload_too_large` | §4.4 |
//! | `ResultTooLarge` | 413 | `result_too_large` | §4.6 |
//! | `ExportTooLarge` | 413 | `export_too_large` | §9.5 |
//! | `RequestTimeout` | 408 | `request_timeout` | §4.4 第 6 步（读取超时） |
//! | `BadHost` | 421 | `bad_host` | §7.2 |
//! | `JobNotFound` | 404 | `job_not_found` | §4.5 |
//! | `Busy` | 503 | `busy` | §4.5 / §8.2 |
//! | `ModelsMissing` | 409 | `models_missing` | §11.1 |
//! | `ModelsCorrupt` | 409 | `models_corrupt` | §11.1 |
//! | `JobNotFinished` | 409 | `job_not_finished` | §4.3 |
//! | `JobEvicted` | 410 | `job_evicted` | §4.5 |
//! | `NotCancellable` | 409 | `not_cancellable` | §4.3 |
//! | `EngineUnavailable` | 503 | `engine_unavailable` | §7.6 |
//! | `InsufficientDiskSpace` | 507 | `insufficient_disk_space` | §11.1 |
//! | `UnsupportedInput` | 422 | `unsupported_input` | §11.1 |
//! | `Download(..)` | 502/504/507/413/409 | 见 [`DownloadErrorMapping`] | §6.1 |
//! | `Ocr(..)` | 见 [`classify_ocr_error`] | 见 [`classify_ocr_error`] | §11.1 |
//! | `Internal` | 500 | `internal` | §11.1 |
//!
//! # 与 §11.1 变体清单的两处偏离（都是文档别处明确要求的行为）
//!
//! - [`ServeError::ExportTooLarge`]：§9.5 要求导出超过 `--max-export-mb` 时返回
//!   **413 `export_too_large`**，而 §11.1 的枚举清单里没有任何变体能产生这个 `code`
//!   （`ResultTooLarge` 的 `code` 是 `result_too_large`，两者是**不同预算**、不同原因）；
//! - [`ServeError::RequestTimeout`]：§4.4 第 6 步要求"有界流式读取 + **读取超时**"，
//!   而 §11.1 的清单里没有能表达 408 的变体；把它降级成 400 会掩盖真实原因。
//!
//! 其余变体与 §11.1 逐项一致。`engine_unavailable` 的 `reason` 字段来自 §7.6。
//!
//! # 下载错误的来源
//!
//! [`DownloadError`] **定义在库里**（`model_store::DownloadError`，十二类，§6.1 第 12 条），
//! serve 侧只有 [`DownloadErrorMapping`] 这一层 HTTP 映射；因此 M0c 报告的接缝
//! （"同一件事有两套错误表示"）在这里被彻底消除。

use rapid_ocr_rs::{DownloadError, RapidOcrError};
use serde::Serialize;

use super::state::OcrAdmission;

/// serve 侧对库 [`DownloadError`] 的 HTTP 映射（§11.1）。
///
/// **错误分类只有一个定义处**：十二类在库里（`model_store::DownloadError`），
/// serve 只补状态码/`code`/`detail`——HTTP 语义不进库（§2.1）。因此 M0c 的
/// "同一件事有两套错误表示"这个接缝被彻底消除，`ServeError::Download` 也不需要
/// 任何字符串匹配。
///
/// 映射规则（§11.1 只给下载规定了三个 `code`，更细的原因进 `detail.kind`）：
///
/// | `DownloadError` | 状态码 | `code` | `detail.kind` |
/// | --- | --- | --- | --- |
/// | `SchemeRejected` / `RedirectRejected` / `HostRejected` / `Network` / `HashMismatch` | 502 | `download_failed` | `scheme` / `redirect` / `host` / `network` / `hash_mismatch` |
/// | `ConnectTimeout` / `ReadTimeout` | 504 | `download_timeout` | `connect_timeout` / `read_timeout` |
/// | `TooLarge` | 413 | `payload_too_large` | `too_large` |
/// | `InsufficientSpace` | 507 | `insufficient_disk_space` | `insufficient_space` |
/// | `Cancelled` | 409 | `download_cancelled` | `cancelled` |
pub trait DownloadErrorMapping {
    /// HTTP 状态码。
    fn status_code(&self) -> u16;
    /// 机器可读的 `code`。
    fn code(&self) -> &'static str;
    /// 人类可读说明（serve 侧措辞：包含 serve 的开关名与排障提示）。
    fn message(&self) -> String;
    /// `detail` 载荷：始终带 `kind`（`kind` 本身来自库，不在这里重复实现），
    /// 其余字段按变体给出。
    fn detail(&self) -> serde_json::Value;
}

impl DownloadErrorMapping for DownloadError {
    fn status_code(&self) -> u16 {
        match self {
            Self::ConnectTimeout { .. } | Self::ReadTimeout { .. } => 504,
            Self::InsufficientSpace { .. } => 507,
            Self::TooLarge { .. } => 413,
            Self::Cancelled => 409,
            Self::SchemeRejected { .. }
            | Self::RedirectRejected { .. }
            | Self::HostRejected { .. }
            | Self::Network { .. }
            | Self::HashMismatch { .. } => 502,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::ConnectTimeout { .. } | Self::ReadTimeout { .. } => "download_timeout",
            Self::InsufficientSpace { .. } => "insufficient_disk_space",
            Self::TooLarge { .. } => "payload_too_large",
            Self::Cancelled => "download_cancelled",
            Self::SchemeRejected { .. }
            | Self::RedirectRejected { .. }
            | Self::HostRejected { .. }
            | Self::Network { .. }
            | Self::HashMismatch { .. } => "download_failed",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::SchemeRejected { scheme } => {
                format!("only https model downloads are allowed, got scheme `{scheme}`")
            }
            Self::RedirectRejected { location } => match location {
                Some(location) => format!(
                    "the model host answered with a redirect to `{location}`; automatic redirects \
                     are disabled"
                ),
                None => "the model host answered with a redirect; automatic redirects are disabled"
                    .to_string(),
            },
            Self::HostRejected { host } => format!(
                "`{host}` is not in the trusted download host allow-list; pass \
                 --allow-download-host to extend it explicitly"
            ),
            Self::TooLarge {
                limit_bytes,
                observed_bytes,
            } => match observed_bytes {
                Some(observed) => format!(
                    "the download is {observed} bytes, which exceeds the {limit_bytes} byte limit"
                ),
                None => format!("the download exceeds the {limit_bytes} byte limit"),
            },
            Self::Network { detail } => format!("the download failed: {detail}"),
            Self::ConnectTimeout { timeout_ms } => {
                format!("the download could not connect within {timeout_ms} ms")
            }
            Self::ReadTimeout { timeout_ms } => {
                format!("the download stalled for more than {timeout_ms} ms while reading")
            }
            Self::InsufficientSpace {
                required_bytes,
                available_bytes,
            } => format!(
                "insufficient disk space: {required_bytes} bytes required, {available_bytes} \
                 bytes available"
            ),
            Self::HashMismatch { expected, actual } => format!(
                "the downloaded file does not match the expected SHA-256 (expected {expected}, got \
                 {actual})"
            ),
            Self::Cancelled => "the download was cancelled at a file boundary".to_string(),
        }
    }

    fn detail(&self) -> serde_json::Value {
        let kind = self.kind();
        match self {
            Self::SchemeRejected { scheme } => {
                serde_json::json!({ "kind": kind, "scheme": scheme })
            }
            Self::RedirectRejected { location } => {
                serde_json::json!({ "kind": kind, "location": location })
            }
            Self::HostRejected { host } => serde_json::json!({ "kind": kind, "host": host }),
            Self::TooLarge {
                limit_bytes,
                observed_bytes,
            } => serde_json::json!({
                "kind": kind,
                "limit_bytes": limit_bytes,
                "observed_bytes": observed_bytes,
            }),
            Self::Network { detail } => serde_json::json!({ "kind": kind, "error": detail }),
            Self::ConnectTimeout { timeout_ms } => {
                serde_json::json!({ "kind": kind, "timeout_ms": timeout_ms })
            }
            Self::ReadTimeout { timeout_ms } => {
                serde_json::json!({ "kind": kind, "timeout_ms": timeout_ms })
            }
            Self::InsufficientSpace {
                required_bytes,
                available_bytes,
            } => serde_json::json!({
                "kind": kind,
                "required_bytes": required_bytes,
                "available_bytes": available_bytes,
            }),
            Self::HashMismatch { expected, actual } => serde_json::json!({
                "kind": kind,
                "expected": expected,
                "actual": actual,
            }),
            Self::Cancelled => serde_json::json!({ "kind": kind }),
        }
    }
}

/// serve 层的统一错误（§11.1）。
///
/// # 三个"协议已冻结、生产者还没到"的变体
///
/// [`Self::ExportTooLarge`]（§9.5 的 `--max-export-mb`）、
/// [`Self::InsufficientDiskSpace`]（§6.5 的磁盘预检）与 [`Self::UnsupportedInput`]
/// （§11.1 的 422；当前所有 `unsupported_input` 都经由 `Ocr(..)` 分类产生，
/// 因此**保留了库侧的错误文本**，见接缝第 5 条）目前没有构造点：它们的状态码与 `code`
/// 由 §11.1 冻结，生产者分别在 M2/M3 与解析层落地。
///
/// 删除它们会让 §11.1 的契约失去覆盖（`every_variant_has_the_documented_status_and_code`
/// 逐项断言了这张表），因此这里用**逐变体**的 `allow` 标明"协议项、生产者未到"——
/// 这与 M0c 那条覆盖整个子树的 `#![allow(dead_code)]` 不是一回事：那条会同时隐藏真正的
/// 未接线代码，这条只作用于三个已冻结的协议变体。
#[derive(Debug)]
pub enum ServeError {
    BadRequest,
    PayloadTooLarge,
    ResultTooLarge,
    /// §9.5：导出文档（含内嵌图片）超过 `--max-export-mb`。**生产者：M3。**
    #[allow(dead_code)]
    ExportTooLarge,
    BadHost,
    BadOrigin,
    Unauthorized,
    /// §4.4 第 6 步：请求体读取超时。
    RequestTimeout,
    Busy,
    JobNotFound,
    JobEvicted,
    JobNotFinished,
    NotCancellable,
    ModelsMissing,
    ModelsCorrupt,
    DownloadsDisabled,
    /// §6.5：下载前的磁盘空间预检。**生产者：M2。**
    #[allow(dead_code)]
    InsufficientDiskSpace,
    /// §11.1 的 422。当前由 `Ocr(RapidOcrError::InvalidImage | InvalidInput | Decode)`
    /// 分类产生（保留库侧原文），因此该变体本身没有构造点。
    #[allow(dead_code)]
    UnsupportedInput,
    EngineUnavailable {
        reason: String,
    },
    Download(DownloadError),
    Ocr(RapidOcrError),
    Internal,
}

/// `RapidOcrError` 的分类结果：状态码 + `code` + 机器可读 kind + 是否把错误文本
/// 作为 `reason` 放进响应体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OcrErrorClass {
    status: u16,
    code: &'static str,
    kind: &'static str,
    reason_is_message: bool,
}

/// `RapidOcrError` → (状态码, `code`, kind) 的**唯一**映射点（§11.1）。
///
/// 逐变体映射，无任何字符串匹配：
///
/// | 库错误 | 状态码 | `code` | 理由 |
/// | --- | --- | --- | --- |
/// | `Config` | 500 | `internal` | 配置在启动期已校验（§7.6 第 2 步），请求期再出现即内部不一致 |
/// | `ModelResolve` | 409 | `models_missing` | 模型文件解析不到 |
/// | `FileNotFound` | 409 | `models_missing` | 同上（模型文件缺失） |
/// | `Download` | 见 [`DownloadErrorMapping`] | 见 [`DownloadErrorMapping`] | 库侧下载器（§6.1）的分类被原样沿用，不降级成字符串 |
/// | `InvalidImage` | 422 | `unsupported_input` | 图片无法解码/超限 |
/// | `InvalidInput` | 422 | `unsupported_input` | 输入不满足契约 |
/// | `Decode` | 422 | `unsupported_input` | 识别结果解码失败（输入导致） |
/// | `Tokenizer` | 409 | `models_corrupt` | 分词器数据是模型集的一部分，失败即模型资产问题 |
/// | `UnsupportedProvider` | 503 | `engine_unavailable` | provider 不可用，必须带 reason（§7.6） |
/// | `UnsupportedBackend` | 503 | `engine_unavailable` | 同上（运行时后端不可用） |
/// | `Io` | 500 | `internal` | 本机 I/O 故障 |
/// | `Yaml` | 500 | `internal` | 配置解析在启动期已完成 |
/// | `Reqwest` | 502 | `download_failed` | 网络栈失败 |
/// | `HashMismatch` | 409 | `models_corrupt` | 文件存在但哈希不符 |
fn classify_ocr_error(error: &RapidOcrError) -> OcrErrorClass {
    match error {
        RapidOcrError::Config(_) => OcrErrorClass {
            status: 500,
            code: "internal",
            kind: "config",
            reason_is_message: false,
        },
        RapidOcrError::ModelResolve(_) => OcrErrorClass {
            status: 409,
            code: "models_missing",
            kind: "model_resolve",
            reason_is_message: false,
        },
        RapidOcrError::FileNotFound(_) => OcrErrorClass {
            status: 409,
            code: "models_missing",
            kind: "file_not_found",
            reason_is_message: false,
        },
        // 库侧下载失败：分类信息不丢——状态码/`code`/`kind` 由**同一套**下载映射给出
        // （例如 ReadTimeout → 504 `download_timeout` / kind `read_timeout`），
        // 而不是被压成 502 + 一个笼统的字符串。
        RapidOcrError::Download(error) => OcrErrorClass {
            status: error.status_code(),
            code: error.code(),
            kind: error.kind(),
            reason_is_message: false,
        },
        RapidOcrError::InvalidImage(_) => OcrErrorClass {
            status: 422,
            code: "unsupported_input",
            kind: "invalid_image",
            reason_is_message: false,
        },
        RapidOcrError::InvalidInput(_) => OcrErrorClass {
            status: 422,
            code: "unsupported_input",
            kind: "invalid_input",
            reason_is_message: false,
        },
        RapidOcrError::Decode(_) => OcrErrorClass {
            status: 422,
            code: "unsupported_input",
            kind: "decode",
            reason_is_message: false,
        },
        RapidOcrError::Tokenizer(_) => OcrErrorClass {
            status: 409,
            code: "models_corrupt",
            kind: "tokenizer",
            reason_is_message: false,
        },
        RapidOcrError::UnsupportedProvider(_) => OcrErrorClass {
            status: 503,
            code: "engine_unavailable",
            kind: "unsupported_provider",
            reason_is_message: true,
        },
        RapidOcrError::UnsupportedBackend(_) => OcrErrorClass {
            status: 503,
            code: "engine_unavailable",
            kind: "unsupported_backend",
            reason_is_message: true,
        },
        RapidOcrError::Io(_) => OcrErrorClass {
            status: 500,
            code: "internal",
            kind: "io",
            reason_is_message: false,
        },
        RapidOcrError::Yaml(_) => OcrErrorClass {
            status: 500,
            code: "internal",
            kind: "yaml",
            reason_is_message: false,
        },
        RapidOcrError::Reqwest(_) => OcrErrorClass {
            status: 502,
            code: "download_failed",
            kind: "reqwest",
            reason_is_message: false,
        },
        RapidOcrError::HashMismatch { .. } => OcrErrorClass {
            status: 409,
            code: "models_corrupt",
            kind: "hash_mismatch",
            reason_is_message: false,
        },
    }
}

impl ServeError {
    /// HTTP 状态码。
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Unauthorized => 401,
            Self::BadOrigin | Self::DownloadsDisabled => 403,
            Self::RequestTimeout => 408,
            Self::JobNotFound => 404,
            Self::JobNotFinished
            | Self::NotCancellable
            | Self::ModelsMissing
            | Self::ModelsCorrupt => 409,
            Self::JobEvicted => 410,
            Self::PayloadTooLarge | Self::ResultTooLarge | Self::ExportTooLarge => 413,
            Self::BadHost => 421,
            Self::UnsupportedInput => 422,
            Self::Busy | Self::EngineUnavailable { .. } => 503,
            Self::InsufficientDiskSpace => 507,
            Self::Download(error) => error.status_code(),
            Self::Ocr(error) => classify_ocr_error(error).status,
            Self::Internal => 500,
        }
    }

    /// 机器可读的 `code`（§11.1 的字符串，逐字一致）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::BadOrigin => "bad_origin",
            Self::DownloadsDisabled => "downloads_disabled",
            Self::RequestTimeout => "request_timeout",
            Self::JobNotFound => "job_not_found",
            Self::JobNotFinished => "job_not_finished",
            Self::JobEvicted => "job_evicted",
            Self::NotCancellable => "not_cancellable",
            Self::ModelsMissing => "models_missing",
            Self::ModelsCorrupt => "models_corrupt",
            Self::PayloadTooLarge => "payload_too_large",
            Self::ResultTooLarge => "result_too_large",
            Self::ExportTooLarge => "export_too_large",
            Self::BadHost => "bad_host",
            Self::UnsupportedInput => "unsupported_input",
            Self::Busy => "busy",
            Self::EngineUnavailable { .. } => "engine_unavailable",
            Self::InsufficientDiskSpace => "insufficient_disk_space",
            Self::Download(error) => error.code(),
            Self::Ocr(error) => classify_ocr_error(error).code,
            Self::Internal => "internal",
        }
    }

    /// 人类可读说明（不含 `code`，便于前端任意排版）。
    pub fn message(&self) -> String {
        match self {
            Self::BadRequest => "the request is malformed".to_string(),
            Self::Unauthorized => "missing or invalid X-RapidOCR-Token".to_string(),
            Self::BadOrigin => {
                "state-changing requests must carry an Origin equal to this server's origin"
                    .to_string()
            }
            Self::DownloadsDisabled => {
                "model downloads are disabled; restart with --allow-download to enable them"
                    .to_string()
            }
            Self::RequestTimeout => {
                "the request body did not arrive within the read timeout".to_string()
            }
            Self::JobNotFound => "no such job".to_string(),
            Self::JobNotFinished => "the job has not finished yet".to_string(),
            Self::JobEvicted => "the job result has been evicted".to_string(),
            Self::NotCancellable => {
                "the job cannot be cancelled; only a job still queued can be cancelled".to_string()
            }
            Self::ModelsMissing => "the model set is incomplete".to_string(),
            Self::ModelsCorrupt => "a model file is present but its content is wrong".to_string(),
            Self::PayloadTooLarge => "the request body exceeds the configured limit".to_string(),
            Self::ResultTooLarge => {
                "the serialized result exceeds the configured limit".to_string()
            }
            Self::ExportTooLarge => {
                "the exported document exceeds the configured limit".to_string()
            }
            Self::BadHost => {
                "the Host header does not belong to this server (possible DNS rebinding)"
                    .to_string()
            }
            Self::UnsupportedInput => {
                "the input image cannot be decoded or exceeds the limits".to_string()
            }
            Self::Busy => "the request queue is full; retry later".to_string(),
            Self::EngineUnavailable { reason } => {
                format!("the OCR engine is not available: {reason}")
            }
            Self::InsufficientDiskSpace => "not enough disk space for the download".to_string(),
            Self::Download(error) => error.message(),
            Self::Ocr(error) => error.to_string(),
            Self::Internal => "internal error".to_string(),
        }
    }

    /// `detail` 载荷。没有额外信息时是 `null`（键仍然存在）。
    pub fn detail(&self) -> serde_json::Value {
        match self {
            Self::EngineUnavailable { reason } => serde_json::json!({ "reason": reason }),
            Self::Download(error) => error.detail(),
            Self::Ocr(error) => {
                let class = classify_ocr_error(error);
                let text = error.to_string();
                let mut detail = serde_json::Map::new();
                detail.insert(
                    "kind".to_string(),
                    serde_json::Value::String(class.kind.to_string()),
                );
                if class.reason_is_message {
                    // §7.6：engine_unavailable 必须带 reason。
                    detail.insert(
                        "reason".to_string(),
                        serde_json::Value::String(text.clone()),
                    );
                }
                detail.insert("error".to_string(), serde_json::Value::String(text));
                serde_json::Value::Object(detail)
            }
            _ => serde_json::Value::Null,
        }
    }

    /// 完整的响应体（三键固定顺序：`code` → `message` → `detail`）。
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code(),
            message: self.message(),
            detail: self.detail(),
        }
    }

    /// 响应体的 JSON 文本。
    pub fn render_body(&self) -> String {
        serde_json::to_string(&self.body()).expect("ErrorBody serialization cannot fail")
    }

    /// `/api/ocr` 的准入结论 → 结果（§7.6）。
    ///
    /// `Run` 与 `Queue` 都是 `Ok(())`：`Loading`/`Rebuilding` 期间新任务**排队而不失败**。
    /// `ModelsMissing` → 409；`Unavailable` → 503 `engine_unavailable{reason}`。
    pub fn from_ocr_admission(admission: OcrAdmission) -> Result<(), Self> {
        match admission {
            OcrAdmission::Run | OcrAdmission::Queue { .. } => Ok(()),
            OcrAdmission::ModelsMissing { .. } => Err(Self::ModelsMissing),
            OcrAdmission::Unavailable { reason } => Err(Self::EngineUnavailable { reason }),
        }
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for ServeError {}

impl From<RapidOcrError> for ServeError {
    /// 保留原始错误：分类只在 [`classify_ocr_error`] 一处完成，
    /// 因此这里不做任何"预翻译"，也不会丢失库侧的诊断文本。
    fn from(error: RapidOcrError) -> Self {
        Self::Ocr(error)
    }
}

impl From<DownloadError> for ServeError {
    fn from(error: DownloadError) -> Self {
        Self::Download(error)
    }
}

/// JSON 错误体：`{ "code", "message", "detail" }`。
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// 机器可读错误码（§11.1）。
    pub code: &'static str,
    /// 人类可读说明。
    pub message: String,
    /// 结构化附加信息；没有时为 `null`。
    pub detail: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rapid_ocr_rs::RapidOcrError;

    use super::{DownloadError, DownloadErrorMapping, ServeError};

    /// 每个变体的 (状态码, code) 都被逐项锁住。
    #[test]
    fn every_variant_has_the_documented_status_and_code() {
        let cases: [(ServeError, u16, &str); 19] = [
            (ServeError::BadRequest, 400, "bad_request"),
            (ServeError::Unauthorized, 401, "unauthorized"),
            (ServeError::BadOrigin, 403, "bad_origin"),
            (ServeError::DownloadsDisabled, 403, "downloads_disabled"),
            (ServeError::RequestTimeout, 408, "request_timeout"),
            (ServeError::JobNotFound, 404, "job_not_found"),
            (ServeError::JobNotFinished, 409, "job_not_finished"),
            (ServeError::JobEvicted, 410, "job_evicted"),
            (ServeError::NotCancellable, 409, "not_cancellable"),
            (ServeError::ModelsMissing, 409, "models_missing"),
            (ServeError::ModelsCorrupt, 409, "models_corrupt"),
            (ServeError::PayloadTooLarge, 413, "payload_too_large"),
            (ServeError::ResultTooLarge, 413, "result_too_large"),
            (ServeError::ExportTooLarge, 413, "export_too_large"),
            (ServeError::BadHost, 421, "bad_host"),
            (ServeError::UnsupportedInput, 422, "unsupported_input"),
            (ServeError::Busy, 503, "busy"),
            (
                ServeError::EngineUnavailable {
                    reason: "no EP".to_string(),
                },
                503,
                "engine_unavailable",
            ),
            (
                ServeError::InsufficientDiskSpace,
                507,
                "insufficient_disk_space",
            ),
        ];
        for (error, status, code) in cases {
            assert_eq!(error.status_code(), status, "error: {error}");
            assert_eq!(error.code(), code, "error: {error}");
            assert!(!error.message().is_empty(), "error: {error}");
        }
        assert_eq!(ServeError::Internal.status_code(), 500);
        assert_eq!(ServeError::Internal.code(), "internal");
    }

    /// 每个下载错误类都有确定的状态码与 code（§6.1 的十类；类型来自库里）。
    #[test]
    fn every_download_error_maps_to_a_documented_status_and_code() {
        let cases: [(DownloadError, u16, &str, &str); 10] = [
            (
                DownloadError::SchemeRejected {
                    scheme: "http".to_string(),
                },
                502,
                "download_failed",
                "scheme",
            ),
            (
                DownloadError::RedirectRejected {
                    location: Some("https://evil.example/m.onnx".to_string()),
                },
                502,
                "download_failed",
                "redirect",
            ),
            (
                DownloadError::HostRejected {
                    host: "evil.example".to_string(),
                },
                502,
                "download_failed",
                "host",
            ),
            (
                DownloadError::TooLarge {
                    limit_bytes: 1024,
                    observed_bytes: Some(2048),
                },
                413,
                "payload_too_large",
                "too_large",
            ),
            (
                DownloadError::Network {
                    detail: "connection reset".to_string(),
                },
                502,
                "download_failed",
                "network",
            ),
            (
                DownloadError::ConnectTimeout { timeout_ms: 10_000 },
                504,
                "download_timeout",
                "connect_timeout",
            ),
            (
                DownloadError::ReadTimeout { timeout_ms: 30_000 },
                504,
                "download_timeout",
                "read_timeout",
            ),
            (
                DownloadError::InsufficientSpace {
                    required_bytes: 100,
                    available_bytes: 1,
                },
                507,
                "insufficient_disk_space",
                "insufficient_space",
            ),
            (
                DownloadError::HashMismatch {
                    expected: "aa".to_string(),
                    actual: "bb".to_string(),
                },
                502,
                "download_failed",
                "hash_mismatch",
            ),
            (
                DownloadError::Cancelled,
                409,
                "download_cancelled",
                "cancelled",
            ),
        ];
        for (download, status, code, kind) in cases {
            assert_eq!(download.status_code(), status, "download: {download}");
            assert_eq!(download.code(), code, "download: {download}");
            assert_eq!(download.kind(), kind, "download: {download}");
            let error = ServeError::Download(download);
            assert_eq!(error.status_code(), status, "error: {error}");
            assert_eq!(error.code(), code, "error: {error}");
            assert_eq!(error.detail()["kind"], kind);
            assert!(!error.message().is_empty(), "error: {error}");
        }
    }

    /// `DownloadError` 的具体数值必须原样进 `detail`（便于定位"超了多少""差多少空间"）。
    #[test]
    fn download_details_carry_the_numbers() {
        let error = ServeError::Download(DownloadError::TooLarge {
            limit_bytes: 1024,
            observed_bytes: Some(4096),
        });
        let detail = error.detail();
        assert_eq!(detail["limit_bytes"], 1024);
        assert_eq!(detail["observed_bytes"], 4096);

        let error = ServeError::Download(DownloadError::InsufficientSpace {
            required_bytes: 600_000_000,
            available_bytes: 12_345,
        });
        let detail = error.detail();
        assert_eq!(detail["required_bytes"], 600_000_000u64);
        assert_eq!(detail["available_bytes"], 12_345);

        let error = ServeError::Download(DownloadError::HashMismatch {
            expected: "aa".to_string(),
            actual: "bb".to_string(),
        });
        assert_eq!(error.detail()["expected"], "aa");
        assert_eq!(error.detail()["actual"], "bb");
        assert!(error.message().contains("SHA-256"), "{}", error.message());
    }

    /// `RapidOcrError` 的每一个变体都在同一张表里映射（无字符串匹配）。
    #[test]
    fn every_rapid_ocr_error_variant_is_mapped() {
        let http = reqwest::blocking::Client::new()
            .get("http://127.0.0.1:99999/")
            .send()
            .expect_err("an out-of-range port must fail before any connection");
        let cases: [(RapidOcrError, u16, &str, &str); 15] = [
            (
                RapidOcrError::Config("bad".to_string()),
                500,
                "internal",
                "config",
            ),
            (
                RapidOcrError::ModelResolve("missing".to_string()),
                409,
                "models_missing",
                "model_resolve",
            ),
            (
                RapidOcrError::FileNotFound(PathBuf::from("det.onnx")),
                409,
                "models_missing",
                "file_not_found",
            ),
            // 库侧下载失败被原样分类：状态码/`code`/`kind` 来自同一套下载映射
            // （`Network` 是"传输失败/非 2xx"这一类，仍是 502 `download_failed`）。
            (
                RapidOcrError::Download(DownloadError::Network {
                    detail: "boom".to_string(),
                }),
                502,
                "download_failed",
                "network",
            ),
            // 超时不降级：连接/读取超时是 504 `download_timeout`，而不是笼统的下载失败。
            (
                RapidOcrError::Download(DownloadError::ReadTimeout { timeout_ms: 30_000 }),
                504,
                "download_timeout",
                "read_timeout",
            ),
            (
                RapidOcrError::InvalidImage("truncated".to_string()),
                422,
                "unsupported_input",
                "invalid_image",
            ),
            (
                RapidOcrError::InvalidInput("huge".to_string()),
                422,
                "unsupported_input",
                "invalid_input",
            ),
            (
                RapidOcrError::Decode("bad".to_string()),
                422,
                "unsupported_input",
                "decode",
            ),
            (
                RapidOcrError::Tokenizer("bad".to_string()),
                409,
                "models_corrupt",
                "tokenizer",
            ),
            (
                RapidOcrError::UnsupportedProvider("not compiled in".to_string()),
                503,
                "engine_unavailable",
                "unsupported_provider",
            ),
            (
                RapidOcrError::UnsupportedBackend("no backend".to_string()),
                503,
                "engine_unavailable",
                "unsupported_backend",
            ),
            (
                RapidOcrError::Io(std::io::Error::other("io")),
                500,
                "internal",
                "io",
            ),
            (
                RapidOcrError::Yaml(serde_yaml::from_str::<u32>("nope").unwrap_err()),
                500,
                "internal",
                "yaml",
            ),
            (
                RapidOcrError::Reqwest(http),
                502,
                "download_failed",
                "reqwest",
            ),
            (
                RapidOcrError::HashMismatch {
                    path: PathBuf::from("det.onnx"),
                    expected: "aa".to_string(),
                    actual: "bb".to_string(),
                },
                409,
                "models_corrupt",
                "hash_mismatch",
            ),
        ];
        for (library_error, status, code, kind) in cases {
            let error = ServeError::from(library_error);
            assert_eq!(error.status_code(), status, "error: {error}");
            assert_eq!(error.code(), code, "error: {error}");
            assert_eq!(error.detail()["kind"], kind, "error: {error}");
        }
    }

    /// §7.6：provider 不可用必须在 OCR 路径上表现为 `engine_unavailable` **且带 reason**。
    #[test]
    fn engine_unavailable_always_carries_a_reason() {
        let error = ServeError::EngineUnavailable {
            reason: "DirectML is unavailable".to_string(),
        };
        assert_eq!(error.detail()["reason"], "DirectML is unavailable");
        assert!(error.message().contains("DirectML is unavailable"));

        let error = ServeError::from(RapidOcrError::UnsupportedProvider(
            "CUDA provider support is not compiled in".to_string(),
        ));
        assert_eq!(error.code(), "engine_unavailable");
        assert_eq!(
            error.detail()["reason"],
            "unsupported provider for v1: CUDA provider support is not compiled in"
        );
    }

    /// 响应体形状固定：三个键始终存在。
    #[test]
    fn error_body_always_has_code_message_and_detail_keys() {
        let bodies = [
            ServeError::BadRequest,
            ServeError::JobEvicted,
            ServeError::Internal,
            ServeError::EngineUnavailable {
                reason: "r".to_string(),
            },
            ServeError::Download(DownloadError::Cancelled),
            ServeError::from(RapidOcrError::InvalidImage("truncated".to_string())),
        ];
        for error in bodies {
            let text = error.render_body();
            let value: serde_json::Value =
                serde_json::from_str(&text).expect("the error body must be valid JSON");
            let object = value.as_object().expect("the body must be an object");
            assert_eq!(
                object.len(),
                3,
                "the body must have exactly code/message/detail: {text}"
            );
            assert_eq!(value["code"], error.code(), "body: {text}");
            assert_eq!(value["message"], error.message(), "body: {text}");
            assert!(object.contains_key("detail"), "body: {text}");
            // 键顺序固定为 code, message, detail（前端/日志可依赖）。
            let code_at = text.find("\"code\"").expect("code key");
            let message_at = text.find("\"message\"").expect("message key");
            let detail_at = text.find("\"detail\"").expect("detail key");
            assert!(
                code_at < message_at && message_at < detail_at,
                "body: {text}"
            );
        }

        let body = ServeError::JobNotFound.body();
        assert_eq!(body.code, "job_not_found");
        assert_eq!(body.message, "no such job");
        assert!(body.detail.is_null(), "unit variants carry `null` detail");
    }

    #[test]
    fn display_includes_the_code_and_the_message() {
        let error = ServeError::Busy;
        let text = error.to_string();
        assert!(text.starts_with("busy: "), "{text}");
        assert!(text.contains("retry later"), "{text}");
    }
}

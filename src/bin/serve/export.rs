//! 标注图与导出文档（§4.2 的 `/annotated.png`、§4.2 的 `/export`、§4.5、§4.6、§9.5）。
//!
//! # 三层职责，逐层都是复用而不是重写
//!
//! 1. **解码**：原图只保留**编码字节**（§4.5）。要生成标注图就重新解码，走库里
//!    `ImageInput::Encoded` 用的**同一个** [`LoadImage`]（编码字节上限、header 像素探测、
//!    EXIF 方向、解码错误语义只有一份实现）；
//! 2. **叠加与文档**：[`draw_output`] 画检测框；JSON / Markdown / HTML 分别由
//!    [`to_output_json`]、[`to_output_markdown`]、[`render_output_report`] 产生。
//!    HTML 用 [`ReportMode::Static`]（正文**不含任何 `<script`**，§9.5 第 1 条），
//!    图片以 `data:image/png;base64,…` 内嵌，因此导出文件脱离服务仍可查看；
//! 3. **体积**：文档在有界写入器里产生（§4.6 / §9.5），超限返回 [`ExportError`]，
//!    由调用方映射成 413 `export_too_large`——**绝不**截断，也绝不发一条图片链接已死的文档。
//!
//! # 为什么标注图只在这里生成
//!
//! `/annotated.png` 与 `export?format=html` 需要**同一张**图（后者把它内嵌进文档），
//! 因此生成与 base64 编码都放在这里，两条路径不会各画一份。

use std::io::Cursor;
use std::sync::Arc;

use image::ImageFormat;
use rapid_ocr_rs::{
    LoadImage, OcrInput, OcrOutput, OcrTimings, RapidOcrError, ReportMode, TextOrder, TimingLedger,
    draw_output, render_output_report, to_output_json, to_output_markdown,
};
use serde_json::{Value, json};

use super::results::{SerializeError, serialize_bounded, text_bounded};

/// 解码原图时的像素/字节上限：与库内 `OcrRequest` 的默认预处理策略同源
/// （§1.1 的 "24 Mpx" 与编码字节上限都只有一份定义）。
fn decode_limits() -> (u64, u64) {
    let policy = rapid_ocr_rs::PreprocessPolicy::default();
    (policy.max_decode_pixels, policy.max_encoded_bytes)
}

/// 导出文档的构建失败（§9.5）。
#[derive(Debug)]
pub(super) enum ExportError {
    /// 文档超过 `--max-export-mb`：`observed_bytes` 是**完整文档**的字节数
    /// （不是截断后的长度），调用方据此给出 413 的两个数值。
    TooLarge { observed_bytes: u64 },
    /// 原图解码失败（编码字节被保留预算释放后，理论上不该再发生解码失败；
    /// 出现即为可定位的库错误）。
    Decode(RapidOcrError),
    /// 库的渲染器报错 / 序列化器内部错误。
    Render(String),
    /// 有界写入器报出的内部错误。
    Internal(String),
}

impl From<RapidOcrError> for ExportError {
    fn from(error: RapidOcrError) -> Self {
        Self::Decode(error)
    }
}

impl From<SerializeError> for ExportError {
    fn from(error: SerializeError) -> Self {
        match error {
            // `text_bounded` / `serialize_bounded` 不知道完整长度时的兜底：
            // 调用方总是先做**投影**检查，因此正常路径不会走到这里。
            SerializeError::TooLarge => Self::TooLarge { observed_bytes: 0 },
            SerializeError::Internal(reason) => Self::Internal(reason),
        }
    }
}

/// 用库的唯一解码实现把保留的**编码字节**变回 `RecImage`。
pub(super) fn decode_original(bytes: &[u8]) -> Result<rapid_ocr_rs::RecImage, ExportError> {
    let (max_pixels, max_encoded) = decode_limits();
    let image = LoadImage::default().load_with_limit(
        OcrInput::Bytes(bytes.to_vec()),
        max_pixels,
        max_encoded,
    )?;
    Ok(image)
}

/// 原图 + 识别结果 → 标注 PNG（`.png` 签名、尺寸 = 原图尺寸）。
///
/// 传进来的是 `Arc<[u8]>`（保留区里的那一份），这里只在生成期间借用它。
pub(super) fn annotated_png(
    original: Arc<[u8]>,
    output: &OcrOutput,
) -> Result<Vec<u8>, ExportError> {
    let image = decode_original(&original)?;
    let annotated = draw_output(&image, output);
    let mut png = Vec::new();
    annotated
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| ExportError::Render(error.to_string()))?;
    Ok(png)
}

/// 导出格式（§9.5 的三个取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExportFormat {
    Json,
    Markdown,
    Html,
}

impl ExportFormat {
    /// 下载文件名用的扩展名（`ocr-<job id>.<ext>`）。
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "md",
            Self::Html => "html",
        }
    }

    /// 响应的 `Content-Type`。
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Json => EXPORT_JSON_CONTENT_TYPE,
            Self::Markdown => EXPORT_MARKDOWN_CONTENT_TYPE,
            Self::Html => EXPORT_HTML_CONTENT_TYPE,
        }
    }
}

/// `data:image/png;base64,…`（§9.5 第 5 条：导出必须是**真正离线可用**的单文件）。
pub(super) fn png_data_url(png: &[u8]) -> String {
    let mut url = String::with_capacity(BASE64_PREFIX.len() + base64_len(png.len()));
    url.push_str(BASE64_PREFIX);
    base64_encode_into(png, &mut url);
    url
}

/// 标注 PNG 的 data URL 前缀（也是 `img-src data:` 允许的形状）。
pub(super) const BASE64_PREFIX: &str = "data:image/png;base64,";

/// base64 编码后的长度（`4 * ceil(n / 3)`，带 `=` 填充）。
fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// 标准 base64（RFC 4648）编码。
///
/// **为什么自己写而不是加依赖**：`docs/05` §2.2 把新增依赖限制在 `tiny_http`／
/// 现有依赖上，而这里需要的只是 20 行、可被逐字节断言的编码（本模块有测试）。
fn base64_encode_into(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let chunks = bytes.len() / 3;
    let remainder = bytes.len() % 3;
    out.reserve(base64_len(bytes.len()) + 3);
    for chunk in bytes[..chunks * 3].as_chunks::<3>().0 {
        let packed = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(ALPHABET[(packed >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(packed >> 12) as usize & 0x3f] as char);
        out.push(ALPHABET[(packed >> 6) as usize & 0x3f] as char);
        out.push(ALPHABET[packed as usize & 0x3f] as char);
    }
    let tail = &bytes[chunks * 3..];
    match remainder {
        1 => {
            let packed = u32::from(tail[0]) << 16;
            out.push(ALPHABET[(packed >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(packed >> 12) as usize & 0x3f] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let packed = (u32::from(tail[0]) << 16) | (u32::from(tail[1]) << 8);
            out.push(ALPHABET[(packed >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(packed >> 12) as usize & 0x3f] as char);
            out.push(ALPHABET[(packed >> 6) as usize & 0x3f] as char);
            out.push('=');
        }
        _ => {}
    }
}

/// 时间账本 + 口径残差 + 自解释文案（§10.6 "复用 timings，不重新测量"）。
///
/// 字段全部由库的 [`TimingLedger`] 计算：`serde` 给出账本的每一个命名分量，
/// 另外三个汇总（`attributed_ms` / `inference_ms` / `rust_ms` / `input_ms`）与
/// `shares` / `conservation` 都调用库的**同一套**方法，serve 不做任何算术。
///
/// `conservation.interpretation` 是库写好的自描述文本（其中明确写出"NOT a strict
/// partition / 口径差异"），诊断面板直接展示它，因此占比不会被误读成严格划分。
pub(super) fn timing_ledger_json(timings: &OcrTimings) -> Value {
    let ledger = TimingLedger::from_timings(timings);
    let mut value = serde_json::to_value(ledger).expect("TimingLedger serialization cannot fail");
    let Value::Object(object) = &mut value else {
        return Value::Null;
    };
    object.insert("attributed_ms".to_string(), json!(ledger.attributed_ms()));
    object.insert("input_ms".to_string(), json!(ledger.input_ms()));
    object.insert("inference_ms".to_string(), json!(ledger.inference_ms()));
    object.insert("rust_ms".to_string(), json!(ledger.rust_ms()));
    object.insert(
        "shares".to_string(),
        match ledger.shares() {
            Some(shares) => serde_json::to_value(shares).expect("LedgerShares serializes"),
            None => Value::Null,
        },
    );
    object.insert(
        "conservation".to_string(),
        serde_json::to_value(ledger.conservation()).expect("LedgerConservation serializes"),
    );
    value
}

/// `/result` 与 `export?format=json` 的**同一份** JSON 值（§4.2 的字段全集）。
///
/// - 基础字段沿用库的 [`to_output_json`]（`regions` / `text` / `items` / `timings` / …）；
/// - `plain_text`：内联页面的"复制全文"（§9.2）读这个名字，值与库的 `text` 同源；
/// - `timing_ledger`：诊断面板的时间账本（M3）。
pub(super) fn result_json(output: &OcrOutput) -> Result<Value, RapidOcrError> {
    let mut value = to_output_json(output)?;
    if let Value::Object(object) = &mut value {
        object.insert(
            "plain_text".to_string(),
            Value::String(output.plain_text(TextOrder::Reading)),
        );
        object.insert(
            "timing_ledger".to_string(),
            timing_ledger_json(&output.timings),
        );
    }
    Ok(value)
}

/// 一次导出的输入（与 `ServeError::ExportTooLarge` 的 `annotated` 字段同源）。
pub(super) struct ExportRequest<'a> {
    pub output: &'a OcrOutput,
    /// 已经生成好的标注 PNG（HTML 内嵌用；json/md 不用）。
    pub png: &'a [u8],
    /// 文档标题（`ocr-<job id>`）。
    pub title: &'a str,
    /// `--max-export-mb` 换算出的字节上限。
    pub limit_bytes: u64,
}

/// 导出文档的字符集后缀：三种格式都是 UTF-8 文本。
pub(super) const EXPORT_JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
pub(super) const EXPORT_MARKDOWN_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";
pub(super) const EXPORT_HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// HTML 导出（§9.5）：静态模式（**无任何脚本**）+ `data:` 内嵌标注图 + 有界写入。
///
/// 两步体积判定，两步都**不截断**：
/// 1. **投影**：先用空 `image_href` 渲染一遍量出正文大小；data URL 只含 base64 字母与
///    `data:image/png;base64,` 前缀（`escape_attr` 不会改动其中任何字符），因此
///    `正文 + data_url` 就是最终长度。超限时**根本不构造**那份大文档；
/// 2. **有界写入**：最终文档仍然经过 [`text_bounded`]，任何意外（转义差异等）都会变成
///    拒绝而不是一条内容缺失的文档。
pub(super) fn html_document(request: &ExportRequest<'_>) -> Result<Vec<u8>, ExportError> {
    let timing = timing_summary(request.output);
    let data_url = png_data_url(request.png);
    let body = render_output_report(
        request.title,
        "",
        request.output,
        &timing,
        ReportMode::Static,
    )
    .map_err(|error| ExportError::Render(error.to_string()))?;
    let projected = (body.len() as u64).saturating_add(data_url.len() as u64);
    if projected > request.limit_bytes {
        return Err(ExportError::TooLarge {
            observed_bytes: projected,
        });
    }
    let document = render_output_report(
        request.title,
        &data_url,
        request.output,
        &timing,
        ReportMode::Static,
    )
    .map_err(|error| ExportError::Render(error.to_string()))?;
    match text_bounded(&document, request.limit_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(SerializeError::TooLarge) => Err(ExportError::TooLarge {
            observed_bytes: document.len() as u64,
        }),
        Err(error) => Err(ExportError::from(error)),
    }
}

/// Markdown 导出：库的 [`to_output_markdown`] + 有界写入。
pub(super) fn markdown_document(request: &ExportRequest<'_>) -> Result<Vec<u8>, ExportError> {
    let document = to_output_markdown(request.output);
    let observed = document.len() as u64;
    match text_bounded(&document, request.limit_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(SerializeError::TooLarge) => Err(ExportError::TooLarge {
            observed_bytes: observed,
        }),
        Err(error) => Err(ExportError::from(error)),
    }
}

/// JSON 导出：与 `/result` **同一份**值（[`result_json`]），因此两个端点的字节数相同。
///
/// `measured_bytes` 是 worker 里已经测过的结果长度（§4.6）：有界写入器在超限时中止，
/// 它本身并不知道完整长度，而那个长度是**已经测过**的事实，直接用它作为 `observed_bytes`。
pub(super) fn json_document(
    output: &OcrOutput,
    limit_bytes: u64,
    measured_bytes: u64,
) -> Result<Vec<u8>, ExportError> {
    let value = result_json(output).map_err(|error| ExportError::Render(error.to_string()))?;
    match serialize_bounded(&value, limit_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(SerializeError::TooLarge) => Err(ExportError::TooLarge {
            observed_bytes: measured_bytes,
        }),
        Err(error) => Err(ExportError::from(error)),
    }
}

/// 报告的计时摘要（与 CLI 的 `report` 用同一句话的措辞）。
pub(super) fn timing_summary(output: &OcrOutput) -> String {
    format!("total {:.1} ms", output.timings.total_ms)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        BASE64_PREFIX, ExportError, ExportRequest, base64_encode_into, base64_len, html_document,
        markdown_document, png_data_url, timing_ledger_json, timing_summary,
    };
    use crate::serve::tests::scripted_output;

    /// 参考实现（RFC 4648 的测试向量 + 边界长度），用来证明自己写的编码是标准 base64。
    fn encode(bytes: &[u8]) -> String {
        let mut out = String::new();
        base64_encode_into(bytes, &mut out);
        out
    }

    #[test]
    fn base64_matches_the_rfc4648_vectors_at_every_length() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        // 0x00/0xFF 边界（位运算最容易在这里写错）。
        assert_eq!(encode(&[0x00]), "AA==");
        assert_eq!(encode(&[0xff]), "/w==");
        assert_eq!(encode(&[0xff, 0xff]), "//8=");
        assert_eq!(encode(&[0xff, 0xff, 0xff]), "////");
        // 长度公式与实现一致（data URL 的**投影**依赖它）。
        for len in 0..64 {
            let bytes: Vec<u8> = (0..len).map(|index| index as u8).collect();
            assert_eq!(encode(&bytes).len(), base64_len(len), "len={len}");
        }
    }

    #[test]
    fn a_png_becomes_a_data_url_that_the_export_csp_allows() {
        let url = png_data_url(&[0x89, b'P', b'N', b'G']);
        assert!(url.starts_with(BASE64_PREFIX), "{url}");
        assert_eq!(url, "data:image/png;base64,iVBORw==");
    }

    #[test]
    fn the_timing_ledger_carries_its_own_interpretation() {
        let output = scripted_output(2, 0);
        let value = timing_ledger_json(&output.timings);
        assert!(value["total_ms"].is_number(), "{value}");
        assert!(value["detector_infer_ms"].is_number(), "{value}");
        assert!(value["attributed_ms"].is_number(), "{value}");
        assert!(value["rust_ms"].is_number(), "{value}");
        assert!(value["conservation"]["residual_ms"].is_number(), "{value}");
        assert!(value["conservation"]["excess_ms"].is_number(), "{value}");
        let interpretation = value["conservation"]["interpretation"]
            .as_str()
            .expect("the ledger must explain itself");
        assert!(
            interpretation.contains("strict partition"),
            "the interpretation must say what the residual is not: {interpretation}"
        );
        assert!(timing_summary(&output).starts_with("total "));
    }

    /// HTML 导出：**无任何 `<script`**、图片是 `data:`、超限是 `TooLarge` 而不是截断。
    #[test]
    fn html_export_is_static_and_refuses_to_overshoot() {
        let output = scripted_output(2, 32);
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let request = ExportRequest {
            output: &output,
            png: &png,
            title: "ocr-job-0",
            limit_bytes: 1 << 20,
        };
        let document = String::from_utf8(html_document(&request).expect("fits")).expect("utf-8");
        assert!(!document.to_lowercase().contains("<script"), "{document}");
        assert!(document.contains(BASE64_PREFIX), "{document}");
        assert!(document.contains("ocr-job-0"), "{document}");
        assert!(document.contains("<style>"), "{document}");

        // 上限刚好不够容纳 data URL → 投影检查直接拒绝，observed 是**完整**文档长度。
        let tight = ExportRequest {
            limit_bytes: 16,
            ..request
        };
        match html_document(&tight).expect_err("must refuse") {
            ExportError::TooLarge { observed_bytes } => {
                assert!(observed_bytes > 16, "{observed_bytes}");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn markdown_export_is_bounded_and_carries_the_text() {
        let output = scripted_output(1, 0);
        let request = ExportRequest {
            output: &output,
            png: &[],
            title: "ocr-job-0",
            limit_bytes: 1 << 20,
        };
        let document =
            String::from_utf8(markdown_document(&request).expect("fits")).expect("utf-8");
        assert!(document.contains("region-0"), "{document}");
        match markdown_document(&ExportRequest {
            limit_bytes: 4,
            ..request
        })
        .expect_err("must refuse")
        {
            ExportError::TooLarge { observed_bytes } => assert!(observed_bytes > 4),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    /// 标注 PNG：`.png` 签名 + 尺寸等于被解码的原图（合成一张真的 PNG 作为输入）。
    #[test]
    fn the_annotated_png_keeps_the_original_dimensions() {
        let source = synthetic_png(37, 19);
        let output = scripted_output(1, 0);
        let png = super::annotated_png(Arc::from(source.into_boxed_slice()), &output)
            .expect("the synthetic image decodes");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
        let decoded = image::load_from_memory(&png).expect("the annotation is a valid image");
        assert_eq!((decoded.width(), decoded.height()), (37, 19));
    }

    /// 合成一张真 PNG（测试输入；`scripted_output` 的 `image` 尺寸与它无关）。
    pub(super) fn synthetic_png(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 256) as u8, 32, 64]));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("the synthetic PNG encodes");
        bytes
    }
}

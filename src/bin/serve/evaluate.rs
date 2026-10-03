//! `POST /api/evaluate`：批量评估已标注样本（M4 的"评估"交付物）。
//!
//! # 复用库的指标，不另写一套
//!
//! 全部指标都来自 `rapid_ocr_rs::evaluation::ocr`：`EvaluationCase`（清单格式）、
//! `evaluate_case`（CER / 精确匹配 / 检测精度与召回 / 多边形 IoU）与
//! `EvaluationSummary::from_cases`（逐例 + 均值）。本模块**只**负责：
//!
//! 1. 解析请求体（白名单式：**恰好**一个 `manifest` 键，没有第二个可以改变行为的字段）；
//! 2. 读清单、按 `--max-eval-cases` 限流、把 `image` 解析成绝对路径（与
//!    `rapidocr evaluate --manifest` **同一套**规则：相对路径相对清单所在目录）；
//! 3. 逐张调引擎（文本管线，与 CLI 的评估一致）并汇总；
//! 4. 把报告交回 HTTP 层。报告字段与 CLI 的 `rapidocr evaluate` 报告**同一份实现**
//!    （`crate::evaluation_report_value`：库的质量指标 + 进程级内存与 ORT 指纹）。
//!
//! # 为什么它不排队
//!
//! §4.1 冻结"识别一律是异步任务"，理由是**单图识别**不该长期占住一个 HTTP 请求。
//! 评估是**批量、无中途交互**的动作：它的结果是一份完整报告，没有"部分报告"这种可轮询的
//! 中间状态，也没有取消语义（推理不可中断，§4.3）。因此这里的选择是：
//!
//! - 请求在**独立线程**里执行、由那个线程写响应（accept 线程立刻回到循环）；
//! - 同时只允许一个评估（第二个请求 503 `busy`，不排队）；
//! - 用例数上限 `--max-eval-cases`（默认 32），让一次请求的时间有上界；
//! - 推理仍走与 OCR 任务**同一条**引擎锁路径，因此它既不会与 OCR 并行破坏引擎，
//!   也不会绕开"会话绝不在 accept 线程上建立"这条规则。
//!
//! 这段偏差是 M4 对 `docs/05` §4.2 的**协议新增**（§4.1 的例外只此一处），已记进文档。

use std::path::{Path, PathBuf};

use rapid_ocr_rs::evaluation::ocr::{
    EvaluationCase, EvaluationReport, EvaluationSummary, evaluate_case,
};
use rapid_ocr_rs::{FormulaPolicy, to_output_items};
use serde_json::{Value, json};

use super::error::ServeError;
use super::server::{EngineLoad, ServeShared, recognize_with, text_request};

/// 判定框匹配的 IoU 阈值：与 `rapidocr evaluate --iou-threshold` 的默认值一致。
///
/// 端点不接受该参数：一个协议字段就应该有一种含义，改阈值请用 CLI（或改这里的常量）。
/// 响应里回显它，报告因此可复核。
pub(super) const IOU_THRESHOLD: f32 = 0.5;

/// `POST /api/evaluate` 的请求体：**恰好**一个 `manifest` 键。
///
/// 白名单式校验（与 `parse_set_id` 同一形状）：任何额外键都被拒绝，而不是被忽略——
/// 这个端点不接受 URL、不接受内联用例、也不接受阈值。
pub(super) fn parse_manifest_body(bytes: &[u8]) -> Result<PathBuf, ServeError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ServeError::BadRequest)?;
    let object = value.as_object().ok_or(ServeError::BadRequest)?;
    if object.len() != 1 {
        return Err(ServeError::BadRequest);
    }
    let raw = object
        .get("manifest")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or(ServeError::BadRequest)?;
    Ok(PathBuf::from(raw))
}

/// 读清单并执行上限判定（**唯一**实现；HTTP 层与单测共用）。
///
/// 错误全部是 [`ServeError::Evaluation`]：`detail.reason` 说明**要改哪个输入**。
pub(super) fn load_cases(manifest: &Path, limit: usize) -> Result<Vec<EvaluationCase>, ServeError> {
    let text = std::fs::read_to_string(manifest).map_err(|error| ServeError::Evaluation {
        reason: format!("cannot read the manifest `{}`: {error}", manifest.display()),
    })?;
    let cases: Vec<EvaluationCase> =
        serde_json::from_str(&text).map_err(|error| ServeError::Evaluation {
            reason: format!(
                "the manifest `{}` is not a `rapidocr evaluate` manifest (expected a JSON array \
                 of `{{ image, text, boxes }}`): {error}",
                manifest.display()
            ),
        })?;
    if cases.is_empty() {
        return Err(ServeError::Evaluation {
            reason: format!("the manifest `{}` lists no cases", manifest.display()),
        });
    }
    if cases.len() > limit {
        return Err(ServeError::Evaluation {
            reason: format!(
                "the manifest `{}` lists {} cases, over the --max-eval-cases limit of {limit}; \
                 split the manifest or raise the limit",
                manifest.display(),
                cases.len()
            ),
        });
    }
    Ok(cases)
}

/// 一个用例的图片路径：相对路径相对**清单所在目录**（与 CLI 一致）。
pub(super) fn resolve_image(manifest: &Path, case: &EvaluationCase) -> PathBuf {
    let image = Path::new(&case.image);
    if image.is_absolute() {
        image.to_path_buf()
    } else {
        manifest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(image)
    }
}

/// 跑一次评估（调用方负责把它放在独立线程里，见模块文档）。
pub(super) fn run(shared: &ServeShared, manifest: &Path) -> Result<Value, ServeError> {
    let cases = load_cases(manifest, shared.max_eval_cases())?;

    // 与 `POST /api/ocr` 同一条准入：模型缺失 → 409（与 `/api/models` 同源同值），
    // 引擎建不起来 → 503 `engine_unavailable`（带 reason）。会话在这里建立（本函数
    // 只在独立线程里被调用），绝不在 accept 线程上。
    match shared.ensure_engine_loaded(false) {
        EngineLoad::Ready => {}
        EngineLoad::BlockedModelsMissing => return Err(shared.models_missing_error()),
        EngineLoad::Failed => {
            return Err(ServeError::EngineUnavailable {
                reason: shared
                    .engine_failure_reason()
                    .unwrap_or_else(|| "the OCR engine could not be created".to_string()),
            });
        }
    }

    let mut reports = Vec::with_capacity(cases.len());
    for case in cases {
        let image = resolve_image(manifest, &case);
        let bytes = std::fs::read(&image).map_err(|error| ServeError::Evaluation {
            reason: format!(
                "cannot read the image `{}` for case `{}`: {error}",
                image.display(),
                case.image
            ),
        })?;
        let request = text_request(
            std::sync::Arc::from(bytes.into_boxed_slice()),
            None,
            // 评估走文本管线：与 `rapidocr evaluate` 一致（公式指标是另一套报告，
            // 见 `evaluation::formula`，不在本端点的范围内）。
            FormulaPolicy::default(),
        );
        let output = recognize_with(shared, request)?;
        let metrics = evaluate_case(
            &case.text,
            &case.boxes,
            &to_output_items(&output),
            IOU_THRESHOLD,
        );
        reports.push(EvaluationReport {
            image: case.image,
            metrics,
        });
    }

    let summary = EvaluationSummary::from_cases(reports);
    let mut report = crate::evaluation_report_value(&summary).map_err(|_| ServeError::Internal)?;
    if let Some(object) = report.as_object_mut() {
        object.insert("iou_threshold".to_string(), json!(IOU_THRESHOLD));
        // 路径脱敏（§7.4）：报告里只留文件名，不回显用户输入的绝对路径。
        object.insert(
            "manifest_file".to_string(),
            json!(
                manifest
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{IOU_THRESHOLD, load_cases, parse_manifest_body, resolve_image};
    use crate::serve::error::ServeError;
    use rapid_ocr_rs::evaluation::ocr::EvaluationCase;

    /// 请求体白名单：**恰好**一个非空 `manifest` 字符串。
    #[test]
    fn the_evaluate_body_carries_only_a_manifest_path() {
        assert_eq!(
            parse_manifest_body(br#"{"manifest":"D:\\m\\golden.json"}"#).expect("ok"),
            std::path::PathBuf::from("D:\\m\\golden.json")
        );
        assert_eq!(
            parse_manifest_body(br#"{"manifest":"  D:\\m\\golden.json  "}"#).expect("trimmed"),
            std::path::PathBuf::from("D:\\m\\golden.json")
        );
        for bad in [
            &br#"{"manifest":""}"#[..],
            &br#"{"manifest":"   "}"#[..],
            &br#"{"manifest":null}"#[..],
            &br#"{"manifest":"a.json","iou_threshold":0.9}"#[..],
            &br#"{"path":"a.json"}"#[..],
            &br#"{}"#[..],
            &br#"[]"#[..],
            &br#"not json"#[..],
        ] {
            assert!(
                matches!(parse_manifest_body(bad), Err(ServeError::BadRequest)),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// 清单加载：读不出来 / 不是清单 / 空清单 / 超过用例上限都是**可定位**的 400。
    #[test]
    fn a_bad_manifest_is_rejected_with_a_locating_reason() {
        let root =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m4-evaluate-tests");
        std::fs::create_dir_all(&root).expect("fixture dir");
        let missing = root.join("definitely-not-here.json");
        let error = load_cases(&missing, 4).expect_err("missing manifest");
        let reason = error.detail()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(reason.contains("definitely-not-here.json"), "{reason}");
        assert_eq!(error.status_code(), 400);
        assert_eq!(error.code(), "bad_request");

        let not_a_manifest = root.join("not-a-manifest.json");
        std::fs::write(&not_a_manifest, b"{\"cases\":[]}").expect("write");
        let reason = load_cases(&not_a_manifest, 4)
            .expect_err("object instead of array")
            .detail()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(reason.contains("not-a-manifest.json"), "{reason}");

        let empty = root.join("empty.json");
        std::fs::write(&empty, b"[]").expect("write");
        let reason = load_cases(&empty, 4).expect_err("empty manifest").detail()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(reason.contains("lists no cases"), "{reason}");

        let three = root.join("three.json");
        std::fs::write(
            &three,
            br#"[{"image":"a.png","text":"a"},{"image":"b.png","text":"b"},{"image":"c.png","text":"c"}]"#,
        )
        .expect("write");
        let cases = load_cases(&three, 3).expect("exactly the limit is allowed");
        assert_eq!(cases.len(), 3);
        let reason = load_cases(&three, 2).expect_err("over the limit").detail()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(reason.contains("--max-eval-cases"), "{reason}");
        assert!(reason.contains('3'), "{reason}");

        std::fs::remove_dir_all(&root).ok();
    }

    /// 相对路径相对清单目录解析，绝对路径原样使用（与 CLI 同一规则）。
    #[test]
    fn case_images_resolve_relative_to_the_manifest() {
        let manifest = Path::new("D:\\models\\golden\\manifest.json");
        let relative = EvaluationCase {
            image: "01.png".to_string(),
            text: "x".to_string(),
            boxes: Vec::new(),
        };
        assert_eq!(
            resolve_image(manifest, &relative),
            Path::new("D:\\models\\golden\\01.png").to_path_buf()
        );
        let absolute = EvaluationCase {
            image: "D:\\images\\02.png".to_string(),
            text: "x".to_string(),
            boxes: Vec::new(),
        };
        assert_eq!(
            resolve_image(manifest, &absolute),
            Path::new("D:\\images\\02.png").to_path_buf()
        );
    }

    #[test]
    fn the_iou_threshold_matches_the_cli_default() {
        assert_eq!(IOU_THRESHOLD, 0.5);
    }
}

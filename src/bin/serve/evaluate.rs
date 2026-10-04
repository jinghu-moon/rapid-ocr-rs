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

/// `--eval-root` 缺失时 `/api/evaluate` 的拒绝（**唯一**措辞）。
///
/// 状态码沿用 §11.1 的"评估请求不合法 → 400 `bad_request` + `detail.reason`"：
/// 这是启动参数的缺失，理由必须点名**要加哪个开关**，而不是一句"评估不可用"。
fn evaluation_disabled() -> ServeError {
    ServeError::Evaluation {
        reason: "POST /api/evaluate is disabled because the server was started without \
                 --eval-root <DIR>; evaluation reads local paths (a manifest and the images it \
                 references) instead of uploaded bytes, so it must be enabled explicitly with a \
                 sandbox directory"
            .to_string(),
    }
}

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

/// `--eval-root` 沙箱：**唯一**允许被 `/api/evaluate` 读取的目录树。
///
/// # 为什么必须有它（评审 P2-3）
///
/// 这个端点是唯一接受**本机路径**的端点（`{"manifest": "<path>"}`），而清单里的
/// `image` 字段又是一个相对清单目录解析的路径。没有沙箱时，一个带 token 的请求可以让
/// 服务读取本机任意 JSON，并按其中的路径读取任意图片（甚至用错误信息把"文件不存在/
/// 不是 JSON"变成存在性预言）。loopback + token 让这不是一个越权入口，但它不是安全的
/// 发布默认值：读取范围必须是**显式配置**出来的。
///
/// 规则（两条都必须成立，任何一条不成立都是可定位错误并点名违规路径）：
///
/// 1. 沙箱本身在启动期规范化：`--eval-root` 必须存在、是目录、且能规范化；
/// 2. 清单与清单引用的每一张图都必须**规范化到**该目录之内——`..`、绝对路径逃逸与
///    符号链接逃逸都会在规范化之后暴露出来，因此不需要（也不能）靠字符串前缀判断。
#[derive(Debug, Clone)]
pub(super) struct EvalRoot {
    root: PathBuf,
}

impl EvalRoot {
    /// 启动期构造（失败即拒绝启动，见 [`super::run::ServeStartError::EvalRoot`]）。
    pub fn new(directory: &Path) -> Result<Self, String> {
        if !directory.is_dir() {
            return Err(format!(
                "{} does not exist or is not a directory",
                directory.display()
            ));
        }
        let root = std::fs::canonicalize(directory)
            .map_err(|error| format!("cannot canonicalize {}: {error}", directory.display()))?;
        Ok(Self { root })
    }

    /// 沙箱根（规范化后的绝对路径；只出现在启动日志与错误文案里）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把一个请求给出的路径解析到沙箱内。`what` 说明这是清单还是某张图（错误文案用）。
    pub fn resolve(&self, requested: &Path, what: &str) -> Result<PathBuf, ServeError> {
        let canonical =
            std::fs::canonicalize(requested).map_err(|error| ServeError::Evaluation {
                reason: format!(
                    "cannot resolve {what} `{}` inside the --eval-root sandbox {}: {error}",
                    requested.display(),
                    self.root.display()
                ),
            })?;
        if !canonical.starts_with(&self.root) {
            return Err(ServeError::Evaluation {
                reason: format!(
                    "{what} `{}` resolves to `{}`, which is outside the --eval-root sandbox {}; \
                     evaluation only reads files inside that directory",
                    requested.display(),
                    canonical.display(),
                    self.root.display()
                ),
            });
        }
        Ok(canonical)
    }

    /// 清单里的一张图：先按 CLI 的同一规则解析成路径，再要求它落在沙箱内。
    pub fn resolve_case(
        &self,
        manifest: &Path,
        case: &EvaluationCase,
    ) -> Result<PathBuf, ServeError> {
        let requested = resolve_image(manifest, case);
        self.resolve(&requested, &format!("the image of case `{}`", case.image))
    }
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
///
/// **沙箱优先**：没有 `--eval-root` 时整体拒绝（在碰任何本机路径之前），配置了沙箱时
/// 清单与每张图都必须规范化到沙箱内（见 [`EvalRoot`]）。
pub(super) fn run(shared: &ServeShared, manifest: &Path) -> Result<Value, ServeError> {
    let root = shared.eval_root().ok_or_else(evaluation_disabled)?.clone();
    let manifest = root.resolve(manifest, "the manifest")?;
    let cases = load_cases(&manifest, shared.max_eval_cases())?;

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
        // 每张图都要重新过沙箱：清单可以先通过，再引用一张沙箱外的图。
        let image = root.resolve_case(&manifest, &case)?;
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

    use super::{EvalRoot, IOU_THRESHOLD, load_cases, parse_manifest_body, resolve_image};
    use crate::serve::error::ServeError;
    use rapid_ocr_rs::evaluation::ocr::EvaluationCase;

    /// 沙箱测试自己的根目录。
    ///
    /// 刻意**不**与 `a_bad_manifest_is_rejected_with_a_locating_reason` 共用
    /// `target/m4-evaluate-tests`：那条用例结束时会删掉整个目录，两个用例并行跑就会互相
    /// 拆台（`cargo test` 默认多线程）。各用各的目录，各自清理。
    fn sandbox_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-evaluate-sandbox")
    }

    /// 评审 P2-3：沙箱本身必须存在且可规范化（否则启动期就失败，而不是运行期才发现）。
    #[test]
    fn the_eval_root_must_be_a_real_directory() {
        let root = sandbox_root().join("root-check");
        std::fs::create_dir_all(&root).expect("fixture dir");
        EvalRoot::new(&root).expect("an existing directory is a valid sandbox");

        let missing = root.join("definitely-not-here");
        let detail = EvalRoot::new(&missing).expect_err("a missing root must be refused");
        assert!(detail.contains("definitely-not-here"), "{detail}");

        let file = root.join("not-a-directory.json");
        std::fs::write(&file, b"[]").expect("write");
        let detail = EvalRoot::new(&file).expect_err("a file is not a sandbox root");
        assert!(detail.contains("not a directory"), "{detail}");
        // 只删自己造的目录：两条沙箱用例并行跑，删公共父目录会互相拆台。
        std::fs::remove_dir_all(sandbox_root().join("root-check")).ok();
    }

    /// 沙箱的包含关系：根内放行、根外拒绝，且错误文案点名违规路径。
    #[test]
    fn the_sandbox_resolves_inside_and_refuses_outside() {
        let root = sandbox_root().join("prefix-check/sandbox");
        std::fs::create_dir_all(root.join("nested")).expect("fixture dirs");
        std::fs::write(root.join("nested/inside.png"), b"x").expect("write");
        // 沙箱根的**同级**目录：字符串前缀比较会误放行（`sandbox-elsewhere` 以 `sandbox`
        // 开头），规范化后的路径包含关系不会。
        let elsewhere = sandbox_root().join("prefix-check/sandbox-elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("fixture dir");
        std::fs::write(elsewhere.join("outside.png"), b"x").expect("write");

        let sandbox = EvalRoot::new(&root).expect("valid sandbox");
        let inside = sandbox
            .resolve(&root.join("nested/inside.png"), "the image")
            .expect("an in-root file is allowed");
        assert!(inside.starts_with(sandbox.root()));

        let error = sandbox
            .resolve(&elsewhere.join("outside.png"), "the image")
            .expect_err("an out-of-root file must be refused");
        let reason = error.detail()["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            reason.contains("outside the --eval-root sandbox"),
            "{reason}"
        );
        assert!(reason.contains("outside.png"), "{reason}");

        // 不存在/越界的路径同样是**可定位**错误，而不是静默跳过。
        let error = sandbox
            .resolve(&root.join("nested/missing.png"), "the image")
            .expect_err("a missing file must be refused");
        assert!(
            error.detail()["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("missing.png"),
            "{}",
            error.detail()
        );

        // 只删自己造的目录（见上一条用例的说明）。
        std::fs::remove_dir_all(sandbox_root().join("prefix-check")).ok();
    }

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

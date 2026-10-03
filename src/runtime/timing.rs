//! 一次页面识别的时间账本：把 `OcrTimings` 拆成**显式命名**的成本项 + 一个显式的
//! **未归属（unattributed）**余量。
//!
//! # 为什么不能只用 `timings.preprocess_ms` 当“Rust 成本”
//!
//! `OcrTimings` 里有两类时间，而且它们**不是**互斥的：
//!
//! - **外层窗口** `preprocess_ms`：`RapidOcrEngine::recognize_text` 从进入预处理到
//!   session 调用之前的整个窗口；
//! - **阶段计时**：detector / classifier / recognizer 各自的 `preprocess_ms` /
//!   `infer_ms` / `postprocess_ms`（它们的和等于 `detect_ms` / `classify_ms` /
//!   `recognize_ms`），以及页面级 `postprocess_ms`。
//!
//! 于是“Rust 成本”如果只取 `page_total.preprocess_ms + page_total.postprocess_ms`，会同时
//! 犯两个错：
//!
//! 1. **漏项**：三个模型各自的 preprocess/postprocess、输入 decode/resize/crop 都没有
//!    单独出现在账本里；
//! 2. **口径错误**：外层 `preprocess_ms` 与阶段计时不是同一层的东西，把它们相加并不是
//!    “Rust 成本”。
//!
//! # 账本的定义
//!
//! 每一项**只算一次**，并且区分“输入侧”与“模型侧”：
//!
//! ```text
//! input_decode_ms + input_resize_ms + input_crop_ms + input_other_ms == preprocess_ms
//! detector_ms  == detector_preprocess_ms  + detector_infer_ms  + detector_postprocess_ms
//! classifier_ms == ...                        （同上）
//! recognizer_ms == ...                        （同上）
//! ```
//!
//! 其中 `input_other_ms = preprocess_ms - decode - resize - crop`，是外层窗口里**没有**
//! 被单独命名的那部分（输入格式转换、EXIF/增强、边界推导、以及 `run()` 内重新计数前的
//! 其它准备工作）。实测在 12 张真实页面上它是一个**不可忽略**的量级（同一台机器、
//! 同一份配置：`preprocess_ms ≈ 380 ms`，其中 `resize_ms ≈ 74 ms`、`crop_ms ≈ 8–20 ms`），
//! 因此把它显式列出来而不是折进“未归属”。
//!
//! # 守恒检查是**判据**，不是断言
//!
//! 保守账本要求 [`crate::api::OcrTimings::total_ms`] 等于上面所有项之和
//! （`attributed_ms`）。本机实测这个等式**不严格成立**：外层 `preprocess_ms` 与阶段计时
//! 的边界跨越了 `run()`（`recognize_text` 的窗口包含整次 `inner.run`，而阶段计时只覆盖
//! `run()` 内部），因此报告里的 `total_ms` 与分量之和不完全可比。
//!
//! 与其把差额藏起来，这里把它作为一等公民：
//!
//! - [`TimingLedger::attributed_ms`]：所有被命名分量之和；
//! - [`TimingLedger::unattributed_ms`]：`total_ms - attributed_ms`（正数 = 漏项，
//!   负数 = 重复计数）；
//! - [`TimingLedger::conservation`]：判定 + 容差 + 残差，直接进报告。
//!
//! `conserved == false` 本身就是一个发现：它说明报告里的 `total_ms` 不能简单当作
//! “各项之和”。任何引用占比的下游都必须先看这个标志。
//!
//! # 守恒检查
//!
//! [`TimingLedger::conservation`] 用**报告里的均值**做检查：所有分量都来自同一批样本的
//! 独立均值，因此它们之间也应当守恒。`rounding_ms` 给出浮点累加造成的量级上限，
//! 超过它就说明账本漏项或双计。

use serde::{Deserialize, Serialize};

use crate::api::OcrTimings;

/// 守恒检查的判定结果（可直接内嵌进 JSON 报告）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerConservation {
    /// 被命名的分量之和。
    pub attributed_ms: f64,
    /// 报告的总时间均值。
    pub total_ms: f64,
    /// `attributed_ms - total_ms`：正数表示漏项，负数表示重复计数。
    pub residual_ms: f64,
    /// 判定阈值：浮点累加与均值舍入的量级上限。
    pub tolerance_ms: f64,
    /// 是否守恒。
    pub conserved: bool,
}

/// 一次识别（或一批同口径采样）的时间账本。
///
/// 字段单位统一为毫秒。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TimingLedger {
    /// 报告的总时间（`OcrTimings::total_ms`）。
    pub total_ms: f64,
    /// 输入解码（`decode_ms`）。
    pub input_decode_ms: f64,
    /// 输入缩放（`resize_ms`）。
    pub input_resize_ms: f64,
    /// 外层预处理窗口里没有被单独命名的部分：
    /// `preprocess_ms - decode_ms - resize_ms - crop_ms`（不小于 0）。
    ///
    /// 实测这个量级不可忽略（12 张页面上约 `preprocess - resize - crop` 的三分之二），
    /// 因此显式列出。它不是“未知误差”，而是“外层窗口的其余部分”。
    pub input_other_ms: f64,
    /// 区域裁剪（`crop_ms`）。
    pub input_crop_ms: f64,
    /// 检测模型 preprocess。
    pub detector_preprocess_ms: f64,
    /// 检测 ONNX Runtime 推理。
    pub detector_infer_ms: f64,
    /// 检测模型 postprocess。
    pub detector_postprocess_ms: f64,
    /// 分类模型 preprocess（未启用时为 0）。
    pub classifier_preprocess_ms: f64,
    /// 分类 ONNX Runtime 推理（未启用时为 0）。
    pub classifier_infer_ms: f64,
    /// 分类模型 postprocess（未启用时为 0）。
    pub classifier_postprocess_ms: f64,
    /// 识别模型 preprocess。
    pub recognizer_preprocess_ms: f64,
    /// 识别 ONNX Runtime 推理。
    pub recognizer_infer_ms: f64,
    /// 识别模型 postprocess（CTC 解码 + word boxes）。
    pub recognizer_postprocess_ms: f64,
    /// 页面级公式路由（检测 + 裁剪 + 公式识别）；未启用时为 0。
    pub formula_ms: f64,
    /// 页面级后处理。
    pub page_postprocess_ms: f64,
    /// `total_ms` 减去上面所有被命名项后的余量（正数 = 未命名，负数 = 重复计数）。
    pub unattributed_ms: f64,
}

impl TimingLedger {
    /// 从一个样本的 [`OcrTimings`] 构造账本。
    pub fn from_timings(timings: &OcrTimings) -> Self {
        let value = |raw: f32| f64::from(raw);
        let decode = value(timings.decode_ms);
        let resize = value(timings.resize_ms);
        let crop = value(timings.crop_ms);
        let outer_preprocess = value(timings.preprocess_ms);
        let input_other = (outer_preprocess - decode - resize - crop).max(0.0);

        let mut ledger = Self {
            total_ms: value(timings.total_ms),
            input_decode_ms: decode,
            input_resize_ms: resize,
            input_other_ms: input_other,
            input_crop_ms: crop,
            detector_preprocess_ms: value(timings.detector_preprocess_ms),
            detector_infer_ms: value(timings.detector_infer_ms),
            detector_postprocess_ms: value(timings.detector_postprocess_ms),
            classifier_preprocess_ms: value(timings.classifier_preprocess_ms),
            classifier_infer_ms: value(timings.classifier_infer_ms),
            classifier_postprocess_ms: value(timings.classifier_postprocess_ms),
            recognizer_preprocess_ms: value(timings.recognizer_preprocess_ms),
            recognizer_infer_ms: value(timings.recognizer_infer_ms),
            recognizer_postprocess_ms: value(timings.recognizer_postprocess_ms),
            formula_ms: value(timings.formula_ms),
            page_postprocess_ms: value(timings.postprocess_ms),
            unattributed_ms: 0.0,
        };
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        ledger
    }

    /// 所有被命名分量之和。
    pub fn attributed_ms(&self) -> f64 {
        self.input_decode_ms
            + self.input_resize_ms
            + self.input_crop_ms
            + self.input_other_ms
            + self.detector_preprocess_ms
            + self.detector_infer_ms
            + self.detector_postprocess_ms
            + self.classifier_preprocess_ms
            + self.classifier_infer_ms
            + self.classifier_postprocess_ms
            + self.recognizer_preprocess_ms
            + self.recognizer_infer_ms
            + self.recognizer_postprocess_ms
            + self.formula_ms
            + self.page_postprocess_ms
    }

    /// 全部 ONNX Runtime 推理时间（三个阶段相加）。
    pub fn inference_ms(&self) -> f64 {
        self.detector_infer_ms + self.classifier_infer_ms + self.recognizer_infer_ms
    }

    /// 全部模型预处理时间（三个阶段相加）。
    pub fn model_preprocess_ms(&self) -> f64 {
        self.detector_preprocess_ms + self.classifier_preprocess_ms + self.recognizer_preprocess_ms
    }

    /// 全部模型后处理时间（三个阶段相加）。
    pub fn model_postprocess_ms(&self) -> f64 {
        self.detector_postprocess_ms
            + self.classifier_postprocess_ms
            + self.recognizer_postprocess_ms
    }

    /// 全部输入侧时间；恒等于 `preprocess_ms`（按构造）。
    pub fn input_ms(&self) -> f64 {
        self.input_decode_ms + self.input_resize_ms + self.input_crop_ms + self.input_other_ms
    }

    /// 全部 Rust 侧时间（输入 + 模型前后处理 + 页面后处理 + 公式路由）。
    ///
    /// **不含** ORT 推理，也不含 `unattributed_ms`。
    pub fn rust_ms(&self) -> f64 {
        self.input_ms()
            + self.model_preprocess_ms()
            + self.model_postprocess_ms()
            + self.page_postprocess_ms
            + self.formula_ms
    }

    /// 守恒检查：`attributed_ms` 是否与报告的 `total_ms` 在容差内一致。
    ///
    /// 容差 = `1e-6 ms` 乘以项数，再与 `total_ms * 1e-6` 取较大者：`total_ms` 本身是
    /// `f32`，大页面上它自己的表示误差就是主要来源。
    ///
    /// **不守恒是一个合法结果**，而且必须被报告出来：外层 `preprocess_ms` 与阶段计时的
    /// 边界跨越了 `inner.run()`，两者相加并不等于 `total_ms`。见模块文档。
    pub fn conservation(&self) -> LedgerConservation {
        let attributed = self.attributed_ms();
        let residual = attributed - self.total_ms;
        let tolerance = (1e-6_f64 * 16.0).max(self.total_ms.abs() * 1e-6);
        LedgerConservation {
            attributed_ms: attributed,
            total_ms: self.total_ms,
            residual_ms: residual,
            tolerance_ms: tolerance,
            conserved: residual.abs() <= tolerance,
        }
    }

    /// 各项占比（分母是 `total_ms`）；`total_ms <= 0` 时返回 `None`。
    pub fn shares(&self) -> Option<LedgerShares> {
        if self.total_ms <= 0.0 {
            return None;
        }
        let share = |value: f64| value / self.total_ms;
        Some(LedgerShares {
            input_share: share(self.input_ms()),
            input_decode_share: share(self.input_decode_ms),
            input_resize_share: share(self.input_resize_ms),
            input_other_share: share(self.input_other_ms),
            input_crop_share: share(self.input_crop_ms),
            model_preprocess_share: share(self.model_preprocess_ms()),
            inference_share: share(self.inference_ms()),
            detector_infer_share: share(self.detector_infer_ms),
            classifier_infer_share: share(self.classifier_infer_ms),
            recognizer_infer_share: share(self.recognizer_infer_ms),
            model_postprocess_share: share(self.model_postprocess_ms()),
            detector_postprocess_share: share(self.detector_postprocess_ms),
            classifier_postprocess_share: share(self.classifier_postprocess_ms),
            recognizer_postprocess_share: share(self.recognizer_postprocess_ms),
            page_postprocess_share: share(self.page_postprocess_ms),
            formula_share: share(self.formula_ms),
            rust_share: share(self.rust_ms()),
            unattributed_share: share(self.unattributed_ms),
        })
    }

    /// 求多个账本的平均值（每个字段独立求均值）。
    ///
    /// **只在非空的输入上调用**；空输入返回全 0 账本。
    pub fn mean(ledgers: &[Self]) -> Self {
        if ledgers.is_empty() {
            return Self::default();
        }
        let divisor = ledgers.len() as f64;
        let sum = |pick: fn(&Self) -> f64| ledgers.iter().map(pick).sum::<f64>() / divisor;
        Self {
            total_ms: sum(|l| l.total_ms),
            input_decode_ms: sum(|l| l.input_decode_ms),
            input_resize_ms: sum(|l| l.input_resize_ms),
            input_other_ms: sum(|l| l.input_other_ms),
            input_crop_ms: sum(|l| l.input_crop_ms),
            detector_preprocess_ms: sum(|l| l.detector_preprocess_ms),
            detector_infer_ms: sum(|l| l.detector_infer_ms),
            detector_postprocess_ms: sum(|l| l.detector_postprocess_ms),
            classifier_preprocess_ms: sum(|l| l.classifier_preprocess_ms),
            classifier_infer_ms: sum(|l| l.classifier_infer_ms),
            classifier_postprocess_ms: sum(|l| l.classifier_postprocess_ms),
            recognizer_preprocess_ms: sum(|l| l.recognizer_preprocess_ms),
            recognizer_infer_ms: sum(|l| l.recognizer_infer_ms),
            recognizer_postprocess_ms: sum(|l| l.recognizer_postprocess_ms),
            formula_ms: sum(|l| l.formula_ms),
            page_postprocess_ms: sum(|l| l.page_postprocess_ms),
            unattributed_ms: sum(|l| l.unattributed_ms),
        }
    }
}

/// 账本各项占比。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LedgerShares {
    pub input_share: f64,
    pub input_decode_share: f64,
    pub input_resize_share: f64,
    pub input_other_share: f64,
    pub input_crop_share: f64,
    pub model_preprocess_share: f64,
    pub inference_share: f64,
    pub detector_infer_share: f64,
    pub classifier_infer_share: f64,
    pub recognizer_infer_share: f64,
    pub model_postprocess_share: f64,
    pub detector_postprocess_share: f64,
    pub classifier_postprocess_share: f64,
    pub recognizer_postprocess_share: f64,
    pub page_postprocess_share: f64,
    pub formula_share: f64,
    pub rust_share: f64,
    pub unattributed_share: f64,
}

#[cfg(test)]
mod tests {
    use super::TimingLedger;
    use crate::api::OcrTimings;

    /// 三个阶段的 preprocess/infer/postprocess 必须与阶段总时间一致，否则
    /// `preprocess_ms` / `detect_ms` 这些“外层”字段就与分阶段字段脱节了。
    fn assert_stage_totals(timings: &OcrTimings) {
        assert!(
            (f64::from(timings.detector_preprocess_ms)
                + f64::from(timings.detector_infer_ms)
                + f64::from(timings.detector_postprocess_ms)
                - f64::from(timings.detect_ms))
            .abs()
                < 1e-3,
            "detector stage breakdown must sum to detect_ms"
        );
        assert!(
            (f64::from(timings.classifier_preprocess_ms)
                + f64::from(timings.classifier_infer_ms)
                + f64::from(timings.classifier_postprocess_ms)
                - f64::from(timings.classify_ms))
            .abs()
                < 1e-3,
            "classifier stage breakdown must sum to classify_ms"
        );
        assert!(
            (f64::from(timings.recognizer_preprocess_ms)
                + f64::from(timings.recognizer_infer_ms)
                + f64::from(timings.recognizer_postprocess_ms)
                - f64::from(timings.recognize_ms))
            .abs()
                < 1e-3,
            "recognizer stage breakdown must sum to recognize_ms"
        );
        assert!(
            (f64::from(timings.preprocess_ms)
                + f64::from(timings.detect_ms)
                + f64::from(timings.classify_ms)
                + f64::from(timings.recognize_ms)
                + f64::from(timings.postprocess_ms)
                + f64::from(timings.formula_ms)
                - f64::from(timings.total_ms))
            .abs()
                < 1e-3,
            "the page-level identity must hold: total = preprocess + det + cls + rec + postprocess \
             (+ formula)"
        );
    }

    /// 全部阶段都启用时的合成样本：账本必须守恒，且每一项都落在预期位置。
    #[test]
    fn ledger_is_conserved_for_a_full_sample() {
        let timings = OcrTimings {
            decode_ms: 5.0,
            resize_ms: 6.0,
            crop_ms: 1.5,
            // 外层窗口包含 decode + resize，再加 2.0 的未单独计时准备。
            preprocess_ms: 13.0,
            detector_preprocess_ms: 3.0,
            detector_infer_ms: 500.0,
            detector_postprocess_ms: 20.0,
            detect_ms: 523.0,
            classifier_preprocess_ms: 1.0,
            classifier_infer_ms: 2.0,
            classifier_postprocess_ms: 0.5,
            classify_ms: 3.5,
            recognizer_preprocess_ms: 4.0,
            recognizer_infer_ms: 280.0,
            recognizer_postprocess_ms: 40.0,
            recognize_ms: 324.0,
            formula_ms: 0.0,
            postprocess_ms: 7.0,
            total_ms: 13.0 + 523.0 + 3.5 + 324.0 + 7.0,
        };
        assert_stage_totals(&timings);

        let ledger = TimingLedger::from_timings(&timings);
        assert_eq!(ledger.input_decode_ms, 5.0);
        assert_eq!(ledger.input_resize_ms, 6.0);
        assert_eq!(
            ledger.input_other_ms, 0.5,
            "the outer preprocess window minus decode/resize/crop is the unnamed remainder"
        );
        assert_eq!(ledger.input_crop_ms, 1.5);
        assert_eq!(
            ledger.input_ms(),
            13.0,
            "input_ms is exactly preprocess_ms (decode + resize + crop + other)"
        );
        assert_eq!(ledger.model_preprocess_ms(), 3.0 + 1.0 + 4.0);
        assert_eq!(ledger.model_postprocess_ms(), 20.0 + 0.5 + 40.0);
        assert_eq!(ledger.inference_ms(), 500.0 + 2.0 + 280.0);
        assert_eq!(
            ledger.rust_ms(),
            13.0 + 8.0 + 60.5 + 7.0,
            "rust = input + model preprocess + model postprocess + page postprocess"
        );
        assert_eq!(ledger.unattributed_ms, 0.0);

        let conservation = ledger.conservation();
        assert!(
            conservation.conserved,
            "the ledger must be conserved: {conservation:?}"
        );
        assert!(conservation.residual_ms.abs() < 1e-9);

        let shares = ledger.shares().expect("a positive total has shares");
        assert!(
            (shares.rust_share + shares.inference_share - 1.0).abs() < 1e-9,
            "in this synthetic sample the ledger accounts for the whole total"
        );
    }

    /// **实测口径回归**：外层预处理窗口与阶段计时跨越了 `inner.run()`，因此现实样本里
    /// `attributed_ms` **不**等于报告的 `total_ms`。这条测试把“不守恒必须被报告成
    /// `conserved = false` + 具体残差”固定下来，避免有人把差额悄悄折进某一项。
    ///
    /// 数字取自本机一次真实运行（第 1 张页面，debug 构建），原样抄自
    /// `examples/probe_timings.rs` 的输出，没有为了好看而调整：
    /// 外层 `preprocess_ms = 380.23`，阶段和 = 1304.06，页面后处理 = 0.07，
    /// 报告 `total_ms = 1779.99`。
    ///
    /// 注意：这条样本**连阶段分解都不完全自洽**（`detector` 三项之和比 `detect_ms`
    /// 小 0.0693 ms），说明现实数据里还有更深一层的计时口径问题；因此这里先按 0.1 ms
    /// 记录该事实，而不是把它抹平。
    #[test]
    fn real_world_outer_window_does_not_conserve_the_reported_total() {
        let timings = OcrTimings {
            decode_ms: 0.0042,
            resize_ms: 74.3112,
            crop_ms: 20.1184,
            preprocess_ms: 380.23,
            detector_preprocess_ms: 37.0154,
            detector_infer_ms: 413.5526,
            detector_postprocess_ms: 28.2756,
            detect_ms: 478.9129,
            classifier_preprocess_ms: 0.0,
            classifier_infer_ms: 0.0,
            classifier_postprocess_ms: 0.0,
            classify_ms: 0.0,
            recognizer_preprocess_ms: 42.3413,
            recognizer_infer_ms: 340.9351,
            recognizer_postprocess_ms: 437.192,
            recognize_ms: 825.0736,
            formula_ms: 0.0,
            postprocess_ms: 0.0716,
            total_ms: 1779.9879,
        };
        let detector_sum = f64::from(timings.detector_preprocess_ms)
            + f64::from(timings.detector_infer_ms)
            + f64::from(timings.detector_postprocess_ms);
        assert!(
            (detector_sum - f64::from(timings.detect_ms)).abs() < 0.1,
            "the real sample's stage breakdown is close but not exact: {detector_sum} vs {}",
            timings.detect_ms
        );
        let ledger = TimingLedger::from_timings(&timings);
        let conservation = ledger.conservation();
        assert!(
            !conservation.conserved,
            "the outer window and the stage timings do not compose into total_ms: {conservation:?}"
        );
        assert!(
            conservation.residual_ms < -50.0,
            "the residual must be reported with its real magnitude, got {}",
            conservation.residual_ms
        );
        // 残差必须等于 unattributed_ms 的相反数——两者是同一件事的两种写法。
        assert!(
            (conservation.residual_ms + ledger.unattributed_ms).abs() < 1e-4,
            "residual and unattributed must agree: {conservation:?} vs {}",
            ledger.unattributed_ms
        );
    }

    /// 阶段关闭（分类器关掉 → 三个分类字段全为 0，且 `classify_ms` 也为 0）时仍然守恒。
    #[test]
    fn ledger_is_conserved_when_a_stage_is_disabled() {
        let timings = OcrTimings {
            decode_ms: 5.0,
            resize_ms: 6.0,
            crop_ms: 0.0,
            preprocess_ms: 11.0,
            detector_preprocess_ms: 3.0,
            detector_infer_ms: 500.0,
            detector_postprocess_ms: 20.0,
            detect_ms: 523.0,
            // 分类器未启用：所有分类字段保持 0。
            classifier_preprocess_ms: 0.0,
            classifier_infer_ms: 0.0,
            classifier_postprocess_ms: 0.0,
            classify_ms: 0.0,
            recognizer_preprocess_ms: 4.0,
            recognizer_infer_ms: 280.0,
            recognizer_postprocess_ms: 40.0,
            recognize_ms: 324.0,
            formula_ms: 0.0,
            postprocess_ms: 7.0,
            total_ms: 11.0 + 523.0 + 0.0 + 324.0 + 7.0,
        };
        assert_stage_totals(&timings);

        let ledger = TimingLedger::from_timings(&timings);
        assert_eq!(ledger.classifier_preprocess_ms, 0.0);
        assert_eq!(ledger.classifier_infer_ms, 0.0);
        assert_eq!(ledger.classifier_postprocess_ms, 0.0);
        assert_eq!(ledger.input_other_ms, 0.0);
        assert_eq!(ledger.input_ms(), 11.0);
        assert_eq!(ledger.inference_ms(), 780.0);
        assert!(ledger.unattributed_ms.abs() < 1e-9, "{ledger:?}");
        assert!(
            ledger.conservation().conserved,
            "a disabled stage must not break conservation"
        );
    }

    /// 公式路由启用时 `formula_ms` 也进入账本，并且仍然守恒。
    #[test]
    fn ledger_accounts_for_formula_routing() {
        let timings = OcrTimings {
            decode_ms: 1.0,
            resize_ms: 2.0,
            crop_ms: 0.5,
            preprocess_ms: 4.0,
            detector_preprocess_ms: 1.0,
            detector_infer_ms: 10.0,
            detector_postprocess_ms: 1.0,
            detect_ms: 12.0,
            classifier_preprocess_ms: 0.0,
            classifier_infer_ms: 0.0,
            classifier_postprocess_ms: 0.0,
            classify_ms: 0.0,
            recognizer_preprocess_ms: 1.0,
            recognizer_infer_ms: 20.0,
            recognizer_postprocess_ms: 2.0,
            recognize_ms: 23.0,
            formula_ms: 50.0,
            postprocess_ms: 3.0,
            total_ms: 4.0 + 12.0 + 0.0 + 23.0 + 3.0 + 50.0,
        };
        assert_stage_totals(&timings);

        let ledger = TimingLedger::from_timings(&timings);
        assert_eq!(ledger.formula_ms, 50.0);
        assert_eq!(ledger.input_other_ms, 0.5);
        assert_eq!(ledger.unattributed_ms, 0.0);
        assert!(ledger.conservation().conserved);
        let shares = ledger.shares().expect("shares");
        assert!(shares.formula_share > 0.5, "formula must dominate here");
    }

    /// **守恒回归**：如果账本漏掉某一项，余量必须把差额暴露出来，而不是被悄悄吸收。
    #[test]
    fn ledger_exposes_a_missing_component_instead_of_hiding_it() {
        let mut ledger = TimingLedger {
            total_ms: 100.0,
            detector_infer_ms: 90.0,
            ..TimingLedger::default()
        };
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        let conservation = ledger.conservation();
        assert_eq!(conservation.residual_ms, -10.0);
        assert!(
            !conservation.conserved,
            "a 10 ms hole must fail the conservation check"
        );

        // 把缺的那一项补上后必须恢复守恒。
        ledger.input_decode_ms = 10.0;
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        assert_eq!(ledger.unattributed_ms, 0.0);
        assert!(ledger.conservation().conserved);
    }

    /// 均值账本必须保持守恒：报告里的占比就是用它算的。
    #[test]
    fn mean_ledger_is_conserved() {
        let sample = |infer: f64, decode: f64| {
            let mut ledger = TimingLedger {
                total_ms: decode + infer,
                detector_infer_ms: infer,
                input_decode_ms: decode,
                ..TimingLedger::default()
            };
            ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
            ledger
        };
        let ledgers = vec![sample(90.0, 10.0), sample(150.0, 50.0), sample(210.0, 90.0)];
        let mean = TimingLedger::mean(&ledgers);
        assert!((mean.total_ms - 200.0).abs() < 1e-9);
        assert!((mean.detector_infer_ms - 150.0).abs() < 1e-9);
        assert!((mean.input_decode_ms - 50.0).abs() < 1e-9);
        assert!(
            mean.conservation().conserved,
            "the mean of conserved ledgers must be conserved: {:?}",
            mean.conservation()
        );
    }

    /// 空输入不能除零。
    #[test]
    fn mean_of_nothing_is_zero() {
        let mean = TimingLedger::mean(&[]);
        assert_eq!(mean, TimingLedger::default());
        assert!(mean.shares().is_none());
    }
}

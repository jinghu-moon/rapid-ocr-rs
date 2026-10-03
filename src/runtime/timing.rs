//! 一次页面识别的时间账本：把 `OcrTimings` 拆成**显式命名**的成本项 + 一个显式的
//! **口径残差（scope residual）**。
//!
//! # 为什么不能只用 `timings.preprocess_ms` 当“Rust 成本”
//!
//! `OcrTimings` 里有两类时间，而且它们**不是**互斥的：
//!
//! - **外层窗口** `preprocess_ms`：`RapidOcrEngine::recognize_text` 从进入预处理
//!   （`rapid_ocr.rs:619`）到 `inner.run()` 调用之前（`rapid_ocr.rs:719`）的整个窗口；
//! - **阶段计时**：detector / classifier / recognizer 各自的 `preprocess_ms` /
//!   `infer_ms` / `postprocess_ms`（它们的和等于 `detect_ms` / `classify_ms` /
//!   `recognize_ms`），以及页面级 `postprocess_ms`。这些都在 `inner.run()`
//!   （`rapid_ocr.rs:106` 起 e2e 计时，`rapid_ocr.rs:130` 结束）内部测量。
//!
//! 于是“Rust 成本”如果只取 `page_total.preprocess_ms + page_total.postprocess_ms`，会同时
//! 犯两个错：
//!
//! 1. **漏项**：三个模型各自的 preprocess/postprocess、输入 decode/resize/crop 都没有
//!    单独出现在账本里；
//! 2. **口径错误**：外层 `preprocess_ms` 与阶段计时不是同一层的东西，把它们相加并不是
//!    “Rust 成本”。
//!
//! # 两个窗口是**串行**的（这是本模块最重要的实现事实）
//!
//! 外层窗口在 `rapid_ocr.rs:619` 开始、`:719` 结束，`inner.run()` 在 `:720` 才被调用。
//! 两者在墙钟上**首尾相接、不重叠**：`total_ms` 的构造是
//! `rapid_ocr.rs:910` 的 `total_ms = exec.e2e_ms + preprocess_ms`，即“外层窗口 + 内层窗口”。
//! 因此**任何“两个窗口重叠、同一段墙钟被算两次”的说法都与实现矛盾**，本模块不得再这样写。
//!
//! # 账本的定义
//!
//! 每一项**只算一次**，并且区分“输入侧”与“模型侧”：
//!
//! ```text
//! input_decode_ms + input_resize_ms + input_crop_ms + input_other_ms == input_ms()
//! input_ms() <= preprocess_ms                       （按构造，见下）
//! detector_ms  == detector_preprocess_ms  + detector_infer_ms  + detector_postprocess_ms
//! classifier_ms == ...                        （同上）
//! recognizer_ms == ...                        （同上）
//! ```
//!
//! 其中 `input_other_ms = max(0, preprocess_ms - decode - resize - crop)`，是外层窗口里
//! **没有**被单独命名的那部分（输入读出、EXIF/增强、BGR 转换、ROI 裁剪等）。实测在 12 张
//! 真实页面上它是一个**不可忽略**的量级（同一台机器、同一份配置：`preprocess_ms ≈ 380 ms`，
//! 其中 `resize_ms ≈ 74 ms`、`crop_ms ≈ 8–20 ms`），因此把它显式列出来而不是折进“未归属”。
//!
//! **`input_ms() <= preprocess_ms` 是按构造成立的**：外层窗口里单独命名的
//! `decode_ms` / `resize_ms` / `crop_ms` 是在 `inner.run()` 的 `prepare_image`
//! （`rapid_ocr.rs:150`）里量的，与外层的 `preprocess_ms` 是**两个口径**，它们之和可以
//! **大于**外层窗口。超出部分**不会被悄悄丢掉**：它被记为
//! [`TimingLedger::input_overflow_ms`]，并入口径残差；`input_ms()` 则按外层窗口截断。
//! 这一选择见 `input_ms()` 的文档。
//!
//! # 守恒检查是**诊断**判据，不是验收证据
//!
//! 这个账本要求 [`crate::api::OcrTimings::total_ms`] 等于上面所有项之和
//! （`attributed_ms`）。本机实测这个等式**不严格成立**。
//!
//! 本机实测（release，12 张真实页面 × 3 轮，`tests/baseline/windows-baseline/bench-cpu.json`）：
//!
//! - `attributed_ms = 979.03`、`total_ms = 985.79` → `residual_ms = attributed - total = −6.75`；
//! - 对应 `excess_ms = 6.75`（= 0.69% 的页面时间）。符号方向是实测的：**命名分量之和比
//!   `total_ms` 少** 6.75 ms，而不是多；
//! - debug 构建下同一残差放大约一个数量级（实测采样在 −55 … −100 ms/页量级，
//!   见 `real_world_outer_window_does_not_conserve_the_reported_total`）。
//!
//! **原因（在代码里定位过，不是推测）**：`inner.run()` 的 e2e 窗口里有一段墙钟时间
//! **落在任何一个“被命名的阶段计时窗口”之外**，因此它进了 `total_ms`（= 外层 + 内层）
//! 却没有进 `attributed_ms`。本机用临时探针（在 `run()` 的每个阶段边界插桩，测完即回滚）
//! 在 12 图上量到的可定位部分：
//!
//! | `inner.run()` 里未被命名的部分 | 代码位置 | 实测量级 |
//! | --- | --- | --- |
//! | `prepare_image` 内的 `resize_image_within_bounds`（记在 `resize_ms`，但 `resize_ms` 不参与任何阶段的 pre/infer/post） | `rapid_ocr.rs:156-162` | ≈ 4.7 ms/页 |
//! | `apply_vertical_padding`（`proc_img.clone()` + 填充，在检测阶段但不在 `detect_ms` 里） | `rapid_ocr.rs:189-195` | 0.8–3.5 ms/页 |
//! | `crop_text_regions`（记在 `crop_ms`，同样不属于任何阶段计时） | `rapid_ocr.rs:210-212` | 1.5–2.5 ms/页 |
//! | recognizer 在三次求和之外的批次装配 / 排序 / bidi | `rec/recognizer.rs:89-98, 222-231` | 0.2–2.1 ms/页 |
//! | `run()` 内阶段之间的其余零头（`e2e_ms` 与各阶段 elapsed 之差） | `rapid_ocr.rs:106-132` | ≈ 0.2 ms/页 |
//!
//! 这些项之和（约 8–13 ms/页）覆盖了实测残差（约 5.3–9.7 ms/页，均值 6.75）。因此这里的
//! 结论是**口径差异**：`total_ms` 是“一个总窗口”，`attributed_ms` 是“若干子窗口之和”，
//! 两者量到的范围不同；**不是**两个窗口重叠。
//!
//! 两种写法指的是同一件事：`residual_ms = attributed_ms - total_ms`，而
//! `TimingLedger::unattributed_ms = total_ms - attributed_ms`。本机实测是
//! `unattributed_ms = +6.75`（与负残差等价）；若残差反号（`unattributed_ms` 为负，目前
//! 尚未实测到），则说明命名分量之和**大于**总额，是方向相反的口径差异。
//!
//! **因此，`conserved = false` 必须这样读**：
//!
//! 1. 残差的含义是**口径差异**（`total_ms` 的总窗口 vs 命名分量之和），不是“时间不见了”，
//!    也不是“`total_ms` 算错了”——`total_ms` 是一次独立的墙钟测量，其数值不受分量口径影响；
//!    并且**不能**反过来把它当成“`total_ms` 高估了”的证据：分量是各自独立测量的，
//!    单凭账本无法判定哪一侧更接近真实用时；
//! 2. 这个账本是**诊断（diagnostic）仪器**：它把各分量的量级摆出来，用来回答“瓶颈在哪一侧”，
//!    而不是一个严格的划分（strict partition）；
//! 3. 账本给出的**占比只能在残差量级内成立**：release 下约 ±0.69%（6.75 ms / 页），
//!    debug 下更大。任何比这个量级更细的性能结论**不能以账本作为验收依据**；
//! 4. 阶段 6 的门槛结论（瓶颈是 ONNX Runtime 而不是 Rust 热路径）不依赖残差：它依据的是
//!    “推理占比比每一个 Rust 分量都大一个数量级”，0.69% 的残差无法推翻这个量级判断。
//!
//! [`LedgerConservation::interpretation`] 会把这句话连同
//! [`LedgerConservation::excess_ms`] 一起写进 JSON，因此读到
//! `conserved = false` 的人不会把它误读成“总量错了”。
//!
//! 更细的单样本证据：本机用过一次性探针（在 `run()` 的每个阶段边界插桩，跑 12 图 × 1 轮，
//! 测完即回滚，仓库里不留代码），配合本文件测试
//! `real_world_outer_window_does_not_conserve_the_reported_total` 固定实测口径。
//!
//! # 守恒检查
//!
//! [`TimingLedger::conservation`] 用**报告里的均值**做检查：所有分量都来自同一批样本的
//! 独立均值，因此它们之间也应当守恒。`tolerance_ms` 给出浮点累加造成的量级上限，
//! 超过它就说明分量之间不构成划分（本机实测是口径差异，见上）。

use serde::{Deserialize, Serialize};

use crate::api::OcrTimings;

/// 守恒检查的判定结果（可直接内嵌进 JSON 报告）。
///
/// **`conserved = false` 不是“总量算错了”。** 本 crate 实测的失败原因是
/// **`total_ms` 与“命名分量之和”的口径不同**（见模块文档），因此这个结构除了判定本身
/// 还带两个人可读/可计算的解释字段：
/// [`LedgerConservation::excess_ms`] 与 [`LedgerConservation::interpretation`]。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerConservation {
    /// 被命名的分量之和。
    pub attributed_ms: f64,
    /// 报告的总时间均值。
    pub total_ms: f64,
    /// `attributed_ms - total_ms`：负数 = 命名分量之和**小于**总额（本机实测的形态，
    /// 原因是内层窗口里有未被命名的墙钟时间），正数 = 命名分量之和**大于**总额
    /// （方向相反的口径差异）。
    pub residual_ms: f64,
    /// 判定阈值：浮点累加与均值舍入的量级上限。
    pub tolerance_ms: f64,
    /// 是否守恒（即分量是否构成 `total_ms` 的严格划分）。
    pub conserved: bool,
    /// 口径残差的量级：`max(0, -residual_ms)`（残差为正、即命名分量之和大于总额时为 `0.0`）。
    ///
    /// 它**不是**“重叠时长”。两个窗口是串行的（`rapid_ocr.rs:619-719` 的外层窗口，
    /// 然后 `rapid_ocr.rs:106-130` 的内层窗口），因此没有任何一段墙钟被算两次。
    /// 这个数字的含义是“账本各分量的占比能用多细”的显式上界。
    ///
    /// 本机实测（release，12 张页面）为 `6.75` ms/页（0.69%）。`conserved = true` 时为 `0.0`。
    ///
    /// **账本的每一项占比只能在 ±`excess_ms` 内成立**，因此这个数字不是可以忽略的尾差。
    pub excess_ms: f64,
    /// 这条判据该怎么读：明确写出“残差是 `total_ms` 的总窗口与命名分量之和之间的
    /// **口径差异**”“两个窗口串行、不是重叠/重复计时”“账本是诊断工具”“占比只在残差量级内
    /// 成立”“不意味着 `total_ms` 算错了”。
    ///
    /// 它随 `conservation` 一起进入 JSON 报告，因此报告是自解释的。
    pub interpretation: String,
}

/// 一次识别（或一批同口径采样）的时间账本。
///
/// 字段单位统一为毫秒。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TimingLedger {
    /// 报告的总时间（`OcrTimings::total_ms` = 外层窗口 + 内层 `inner.run()` 窗口）。
    pub total_ms: f64,
    /// 外层窗口 `OcrTimings::preprocess_ms` 的原始值。
    ///
    /// 它**不在** [`TimingLedger::attributed_ms`] 的求和里（否则会与
    /// [`TimingLedger::input_other_ms`] 重复）；它的作用是给
    /// [`TimingLedger::input_ms`] 提供截断上界，并让
    /// [`TimingLedger::input_overflow_ms`] 可被复算。
    pub input_preprocess_ms: f64,
    /// 输入解码（`decode_ms`）。
    pub input_decode_ms: f64,
    /// 输入缩放（`resize_ms`）。
    pub input_resize_ms: f64,
    /// 外层预处理窗口里没有被单独命名的部分：
    /// `max(0, preprocess_ms - decode_ms - resize_ms - crop_ms)`。
    ///
    /// 实测这个量级不可忽略（12 张页面上约 `preprocess - resize - crop` 的三分之二），
    /// 因此显式列出。它不是“未知误差”，而是“外层窗口的其余部分”。
    pub input_other_ms: f64,
    /// 区域裁剪（`crop_ms`）。
    pub input_crop_ms: f64,
    /// 输入侧“被单独命名的三项”中，外层窗口**真的覆盖到**的那部分：
    /// `decode + resize + crop - input_overflow_ms`（也就是被截断到外层窗口剩余预算内）。
    ///
    /// `decode_ms` / `resize_ms` / `crop_ms` 是在 `inner.run()` 里量的（与外层窗口不同口径），
    /// 三者之和**可以大于**外层窗口；超出部分见 [`TimingLedger::input_overflow_ms`]。
    /// 本字段让 `input_named_within_ms + input_other_ms <= input_preprocess_ms` 成为
    /// **按构造**成立的不变量，而不是靠 `.max(0.0)` 掩盖。
    pub input_named_within_ms: f64,
    /// 输入侧三项之和超出外层窗口的量：
    /// `max(0, decode + resize + crop - preprocess_ms)`。
    ///
    /// **不会被丢掉**：它入口径残差（见 [`TimingLedger::conservation`]），
    /// 也就是被算进 `attributed_ms` 而处在外层窗口之外的量。
    pub input_overflow_ms: f64,
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
    /// `total_ms` 减去上面所有被命名项后的余量（正数 = 报告的总时间里有一部分没有被任何
    /// 命名分量覆盖；本机实测的形态，release ≈ +6.75 ms/页）。
    ///
    /// 它的**口径**含义是：`total_ms` 是“外层窗口 + 内层 `inner.run()` 窗口”，而命名分量只是
    /// 这些窗口里的若干子窗口；内层窗口里未被命名的墙钟（`prepare_image` 的 resize、
    /// `apply_vertical_padding`、`crop_text_regions`、recognizer 的批次装配等）只能落在
    /// 这个余量里。**两个窗口是串行的，这里没有重复计时**；也**不代表 `total_ms` 本身有错误**
    /// ——分量是各自独立测量的，单凭账本无法判定哪一侧更接近真实用时。见模块文档。
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
        // 输入侧三项之和与外层窗口是两个口径，前者可以更大。超出部分不丢：它被记为
        // `input_overflow_ms` 并入口径残差；`input_named_within_ms` 是被外层窗口覆盖的那部分。
        let named_input = decode + resize + crop;
        let input_budget = (outer_preprocess - input_other).max(0.0);
        let input_named_within = named_input.min(input_budget);
        let input_overflow = (named_input - input_budget).max(0.0);

        let mut ledger = Self {
            total_ms: value(timings.total_ms),
            input_preprocess_ms: outer_preprocess,
            input_decode_ms: decode,
            input_resize_ms: resize,
            input_other_ms: input_other,
            input_crop_ms: crop,
            input_named_within_ms: input_named_within,
            input_overflow_ms: input_overflow,
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
    ///
    /// 输入侧用的是**完整的**三项（`input_named_within_ms + input_overflow_ms`），因为
    /// [`TimingLedger::input_ms`] 只是在“外层窗口能不能覆盖到”这个口径上截断；被截断掉的那部分
    /// 输入时间同样是真实被测量到的命名分量，必须留在和里，从而留在口径残差里（否则残差会
    /// 系统性偏小，等于把超出量丢掉）。
    pub fn attributed_ms(&self) -> f64 {
        self.input_named_within_ms
            + self.input_overflow_ms
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

    /// 全部输入侧时间。
    ///
    /// **不变量（按构造成立）**：`input_ms() <= input_preprocess_ms`（= `preprocess_ms`）。
    ///
    /// 为什么这样定义：`input_other_ms` 是“外层窗口里没被单独命名的余量”，它按定义不能为负；
    /// 而 `input_decode_ms` / `input_resize_ms` / `input_crop_ms` 是**另一个口径**的测量
    /// （`inner.run()` 的 `prepare_image` 内），三者之和允许大于外层窗口。若直接把
    /// `decode + resize + crop + input_other` 相加，就会出现“账本声称的输入时间超过了
    /// 它唯一能观测输入时间的那个窗口测到的时长”，也就是 `input_ms() > preprocess_ms`。
    ///
    /// 本实现的选择是：**输入侧只认外层窗口覆盖到的那部分**——记
    /// `input_named_within_ms = min(decode + resize + crop, 外层窗口剩余预算)`，超出部分
    /// **不丢弃**，而是作为 [`TimingLedger::input_overflow_ms`] 公开，并因此留在
    /// [`TimingLedger::conservation`] 的残差里（读者能看到“有多达这么多输入侧时间是外层窗口
    /// 没量到的”）。
    ///
    /// 这不是“把冲突藏起来”：隐藏的做法是继续用 `.max(0.0)` 让 `input_other_ms` 归零、
    /// 同时让 `input_ms()` 悄悄超出 `preprocess_ms`；这里把超出量变成了一个显式字段 + 一个
    /// 可断言的构造性不变量，并且**没有**改动 `total_ms` / `preprocess_ms` 的任何语义。
    pub fn input_ms(&self) -> f64 {
        // `input_named_within_ms` 已被截断到外层窗口的剩余预算，因此这个和不会超过窗口。
        self.input_named_within_ms + self.input_other_ms
    }

    /// 全部 Rust 侧时间（输入 + 模型前后处理 + 页面后处理 + 公式路由）。
    ///
    /// **不含** ORT 推理，也不含 `unattributed_ms`。
    ///
    /// 输入侧用的是截断后的 [`TimingLedger::input_ms`]（外层窗口覆盖到的部分），因此
    /// `rust_ms() + inference_ms()` 在 `input_overflow_ms > 0` 时**小于**
    /// [`TimingLedger::attributed_ms`]，差额正好是那个超出量——它属于“外层窗口没量到的输入
    /// 时间”，不应当被算进“外层窗口内的 Rust 成本”。见 [`TimingLedger::input_ms`]。
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
    /// **不守恒是一个合法结果**，而且必须被报告出来：`total_ms` 是“外层窗口 + 内层
    /// `inner.run()` 窗口”的总窗口，而命名分量只是这些窗口里的若干子窗口（内层窗口里的
    /// resize / padding / crop / 批次装配都不属于任何阶段计时），两者口径不同。返回值里的
    /// [`LedgerConservation::excess_ms`] 与 [`LedgerConservation::interpretation`] 就是
    /// 为了让“不守恒”不被读成“总量算错了”或“窗口重叠”。见模块文档。
    ///
    /// [`TimingLedger::input_overflow_ms`] 这种“命名分量超出外层窗口”的量也留在这个残差里，
    /// 因此残差同时承载两个方向的记账差。
    pub fn conservation(&self) -> LedgerConservation {
        let attributed = self.attributed_ms();
        let residual = attributed - self.total_ms;
        let tolerance = (1e-6_f64 * 16.0).max(self.total_ms.abs() * 1e-6);
        let conserved = residual.abs() <= tolerance;
        LedgerConservation {
            attributed_ms: attributed,
            total_ms: self.total_ms,
            residual_ms: residual,
            tolerance_ms: tolerance,
            conserved,
            excess_ms: (-residual).max(0.0),
            interpretation: conservation_interpretation(residual, self.total_ms, tolerance),
        }
    }

    /// 各项占比（分母是 `total_ms`）；`total_ms <= 0` 时返回 `None`。
    ///
    /// 占比**只在残差量级内成立**：本机实测 release 下为 ±0.69%（6.75 ms/页），debug 下更大。
    /// 引用这些占比前先看 [`TimingLedger::conservation`] 的
    /// [`LedgerConservation::excess_ms`] / [`LedgerConservation::interpretation`]。
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
            input_preprocess_ms: sum(|l| l.input_preprocess_ms),
            input_decode_ms: sum(|l| l.input_decode_ms),
            input_resize_ms: sum(|l| l.input_resize_ms),
            input_other_ms: sum(|l| l.input_other_ms),
            input_crop_ms: sum(|l| l.input_crop_ms),
            input_named_within_ms: sum(|l| l.input_named_within_ms),
            input_overflow_ms: sum(|l| l.input_overflow_ms),
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

/// 生成 [`LedgerConservation::interpretation`] 的文案。
///
/// 文本会原样进入 JSON 报告，因此用英文写（报告里的其它 `basis` / `note` 字段也是英文）。
///
/// **硬约束**：这段文案**不得**声称两个计时窗口重叠/同一段墙钟被算两次。实现事实是
/// 外层窗口（`rapid_ocr.rs:619-719`）与 `inner.run()` 的 e2e 窗口（`rapid_ocr.rs:106-130`）
/// **串行**，`total_ms = exec.e2e_ms + preprocess_ms`（`rapid_ocr.rs:910`）。
fn conservation_interpretation(residual_ms: f64, total_ms: f64, tolerance_ms: f64) -> String {
    let share = |value: f64| {
        if total_ms > 0.0 {
            value / total_ms * 100.0
        } else {
            0.0
        }
    };
    if residual_ms.abs() <= tolerance_ms {
        format!(
            "conserved: the named components form a strict partition of total_ms for this \
             sample, to within the {tolerance_ms:.6} ms rounding tolerance."
        )
    } else if residual_ms < 0.0 {
        let gap_ms = -residual_ms;
        format!(
            "NOT a strict partition, and NOT a wrong total either: this is a SCOPE difference \
             between total_ms and the sum of its named parts. residual_ms = {residual_ms:.6}, \
             i.e. the named components sum to {gap_ms:.6} ms ({share:.2}% of total_ms) LESS than \
             total_ms. The two timing windows are SEQUENTIAL and do not overlap: the outer \
             OcrTimings::preprocess_ms window runs from rapid_ocr.rs:619 to :719 and \
             inner.run() is only entered at :720, whose own end-to-end window runs from \
             rapid_ocr.rs:106 to :130; total_ms is their sum \
             (total_ms = exec.e2e_ms + preprocess_ms, rapid_ocr.rs:910). No wall-clock interval \
             is therefore counted twice. What the residual measures is the part of inner.run()'s \
             own window that falls outside every named stage window and therefore has no \
             component in this ledger: the resize inside prepare_image \
             (rapid_ocr.rs:156-162), apply_vertical_padding (rapid_ocr.rs:189-195), \
             crop_text_regions (rapid_ocr.rs:210-212), and the recognizer's batch assembly \
             (rec/recognizer.rs:89-98, 222-231). total_ms is a single wall-clock measurement and \
             is unaffected by how the parts are named; conversely, because the parts were \
             measured independently, this ledger alone cannot decide which side is closer to \
             the real elapsed time, so it does NOT claim that total_ms overstates. This ledger \
             is a DIAGNOSTIC instrument, not acceptance evidence: every share is valid only to \
             within {gap_ms:.6} ms ({share:.2}% of total_ms), and the stage-6 conclusion (ONNX \
             Runtime inference is an order of magnitude larger than every Rust component) is not \
             affected by a residual of this size.",
            share = share(gap_ms),
        )
    } else {
        format!(
            "NOT a strict partition: residual_ms = {residual_ms:.6} is positive, i.e. the named \
             components sum to MORE than the reported total_ms by {residual_ms:.6} ms \
             ({share:.2}% of total_ms). This is a SCOPE difference in the opposite direction (a \
             component whose measurement scope reaches outside the window that total_ms covers, \
             or a component measured twice relative to it), not a statement about the accuracy \
             of total_ms, and it says nothing about the two windows overlapping: they are \
             sequential (rapid_ocr.rs:619-719 then :106-130). This ledger is a DIAGNOSTIC \
             instrument, not acceptance evidence: every share is valid only to within \
             {residual_ms:.6} ms ({share:.2}% of total_ms).",
            share = share(residual_ms),
        )
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
            ledger.input_named_within_ms, 12.5,
            "all three named input parts fit inside the outer window budget (13.0 - 0.5 other)"
        );
        assert_eq!(
            ledger.input_overflow_ms, 0.0,
            "nothing overflows the outer window in this sample"
        );
        assert_eq!(
            ledger.input_ms(),
            13.0,
            "input_ms is exactly preprocess_ms (the named three + the unnamed remainder)"
        );
        assert!(
            ledger.input_ms() <= ledger.input_preprocess_ms,
            "the input invariant must hold by construction: {} <= {}",
            ledger.input_ms(),
            ledger.input_preprocess_ms
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

    /// **实测口径回归**：`total_ms` 是“外层 `preprocess_ms` 窗口 + 内层 `inner.run()` 窗口”
    /// 的总窗口，而命名分量只是这些窗口里的若干子窗口，因此现实样本里 `attributed_ms`
    /// **不**等于报告的 `total_ms`。这条测试把“不守恒必须被报告成 `conserved = false` +
    /// 具体残差 + **口径差异解释**”固定下来，避免有人把差额悄悄折进某一项，
    /// 也避免有人把 `conserved = false` 读成“总量算错了”或“窗口重叠”。
    ///
    /// 数字取自本机一次真实运行（第 1 张页面，debug 构建），原样抄自当时的探针输出，
    /// 没有为了好看而调整：外层 `preprocess_ms = 380.23`，阶段和 = 1304.06，
    /// 页面后处理 = 0.07，报告 `total_ms = 1779.99`。
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
        // 负残差必须被**解释成口径差异**，而不是“总量算错了”、更不是“窗口重叠”。
        assert!(
            (conservation.excess_ms + conservation.residual_ms).abs() < 1e-9,
            "a negative residual means the named parts sit inside the total window, so excess_ms \
             must be |residual|: {conservation:?}"
        );
        assert!(
            conservation.excess_ms > 50.0,
            "this sample's scope residual must keep its real magnitude: {conservation:?}"
        );
        let text = &conservation.interpretation;
        for needle in [
            "NOT a strict partition",
            "SCOPE difference",
            "DIAGNOSTIC instrument, not acceptance evidence",
            "valid only to within",
            "unaffected",
        ] {
            assert!(
                text.contains(needle),
                "the interpretation must state `{needle}` so `conserved = false` cannot be \
                 mistaken for a wrong total: {text}"
            );
        }
        assert!(
            text.contains(&format!("residual_ms = {:.6}", conservation.residual_ms)),
            "the interpretation must quote the actual residual: {text}"
        );
    }

    /// **因果陈述锁定**：解释文案必须说清“两个计时窗口**串行**、不是重叠/重复计时”，
    /// 必须给出残差量级与“不是验收依据”的框架，并且**不得**出现任何“重叠”的说法。
    ///
    /// 这条测试存在的原因：旧文案把负残差写成“两个窗口重叠、同一段墙钟被算两次”，
    /// 而实现里外层窗口（`rapid_ocr.rs:619-719`）与 `inner.run()` 的 e2e 窗口
    /// （`rapid_ocr.rs:106-130`）是首尾相接的，`total_ms = exec.e2e_ms + preprocess_ms`
    /// （`rapid_ocr.rs:910`）。文案与实现矛盾时必须在测试里被拦住。
    #[test]
    fn interpretation_states_sequential_windows_and_never_claims_overlap() {
        // 用真实量级的合成样本，命中的是“命名分量之和小于 total_ms”这一支。
        let mut ledger = TimingLedger {
            total_ms: 1000.0,
            input_preprocess_ms: 80.0,
            input_named_within_ms: 8.0,
            detector_infer_ms: 900.0,
            ..TimingLedger::default()
        };
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        let conservation = ledger.conservation();
        assert!(!conservation.conserved, "{conservation:?}");
        assert_eq!(conservation.residual_ms, -92.0);
        assert_eq!(conservation.excess_ms, 92.0);
        assert_eq!(
            ledger.unattributed_ms, 92.0,
            "unattributed_ms is the opposite sign of residual_ms"
        );

        let text = &conservation.interpretation;
        for needle in [
            "residual_ms = -92.000000",
            "the named components sum to 92.000000 ms",
            "SEQUENTIAL and do not overlap",
            "rapid_ocr.rs:619 to :719",
            "rapid_ocr.rs:106 to :130",
            "rapid_ocr.rs:910",
            "No wall-clock interval is therefore counted twice",
            "SCOPE difference",
            "prepare_image",
            "apply_vertical_padding",
            "crop_text_regions",
            "recognizer.rs:89-98",
            "DIAGNOSTIC instrument, not acceptance evidence",
            "valid only to within 92.000000 ms",
            "9.20% of total_ms",
        ] {
            assert!(
                text.contains(needle),
                "the interpretation must state `{needle}`: {text}"
            );
        }
        // 关键否定断言：不得再出现旧文案里的“重叠 / 重复计数”因果说法。
        // （“do not overlap”“counted twice”是**否定**表述，正是要保留的部分。）
        for forbidden in [
            "windows overlap",
            "the same wall-clock interval is counted in two",
            "is duplicated",
            "cross the inner run() boundary",
        ] {
            assert!(
                !text.contains(forbidden),
                "the interpretation must NOT claim `{forbidden}` (the windows are sequential): \
                 {text}"
            );
        }
    }

    /// 正残差（命名分量之和大于总额）必须得到**另一种**解释文案：它同样不是“总量错了”，
    /// 但方向相反，不能复用负残差的说法，也不得声称窗口重叠。
    #[test]
    fn positive_residual_is_explained_as_a_different_direction() {
        let mut ledger = TimingLedger {
            total_ms: 100.0,
            detector_infer_ms: 110.0,
            ..TimingLedger::default()
        };
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        let conservation = ledger.conservation();
        assert_eq!(conservation.residual_ms, 10.0);
        assert!(!conservation.conserved);
        assert_eq!(
            conservation.excess_ms, 0.0,
            "excess_ms is max(0, -residual), so a positive residual reports 0.0"
        );
        assert!(
            conservation.interpretation.contains("is positive"),
            "unexpected interpretation: {}",
            conservation.interpretation
        );
        assert!(
            conservation
                .interpretation
                .contains("sum to MORE than the reported total_ms by 10.000000 ms"),
            "a positive residual must state its direction and magnitude: {}",
            conservation.interpretation
        );
        assert!(
            !conservation
                .interpretation
                .contains("the same wall-clock interval is counted in two"),
            "a positive residual must not be described as overlapping windows: {}",
            conservation.interpretation
        );
        assert!(
            conservation
                .interpretation
                .contains("they are sequential (rapid_ocr.rs:619-719 then :106-130)"),
            "a positive residual must still cite the sequential window relationship: {}",
            conservation.interpretation
        );
    }

    /// **P2-c 分支覆盖**：`decode + resize + crop > preprocess_ms` 时，
    /// `input_ms() <= preprocess_ms` 必须**按构造**仍然成立，超出量必须被显式记为
    /// [`TimingLedger::input_overflow_ms`]（而不是被 `.max(0.0)` 悄悄吞掉），
    /// 并且必须体现在口径残差里。
    ///
    /// 这个分支不是虚构的：`decode_ms` / `resize_ms` / `crop_ms` 在 `inner.run()` 内测量
    /// （`rapid_ocr.rs:150-178, 210-212`），外层 `preprocess_ms` 是另一个窗口
    /// （`rapid_ocr.rs:619-719`），两者口径不同，前者之和完全可以更大
    /// （例如 `Pixels` 输入走 `to_bgr` 而外层不做解码）。
    #[test]
    fn input_overflow_keeps_the_input_invariant_by_construction() {
        let timings = OcrTimings {
            decode_ms: 2.0,
            resize_ms: 7.0,
            crop_ms: 3.0,
            // 外层窗口只量到 5 ms，而命名三项之和是 12 ms。
            preprocess_ms: 5.0,
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
            formula_ms: 0.0,
            postprocess_ms: 3.0,
            total_ms: 5.0 + 12.0 + 0.0 + 23.0 + 3.0,
        };
        let ledger = TimingLedger::from_timings(&timings);

        assert_eq!(
            ledger.input_other_ms, 0.0,
            "when the named three exceed the window there is no unnamed remainder"
        );
        assert_eq!(
            ledger.input_overflow_ms, 7.0,
            "the 7 ms of named input time outside the outer window must be explicit"
        );
        assert_eq!(
            ledger.input_named_within_ms, 5.0,
            "only the part the outer window covers is counted as input"
        );
        assert_eq!(
            ledger.input_ms(),
            5.0,
            "input_ms is clipped to the outer window: decode + resize + crop - overflow"
        );
        assert!(
            ledger.input_ms() <= ledger.input_preprocess_ms,
            "the input invariant must hold by construction even in the overflow branch: \
             {} <= {}",
            ledger.input_ms(),
            ledger.input_preprocess_ms
        );
        assert!(
            ledger.input_decode_ms + ledger.input_resize_ms + ledger.input_crop_ms
                > ledger.input_preprocess_ms,
            "this sample must really exercise the overflow branch"
        );

        // 超出量必须留在残差里，而不是被丢掉：残差 = 三项之和 + 阶段 - total。
        let raw_named = ledger.input_decode_ms + ledger.input_resize_ms + ledger.input_crop_ms;
        let conservation = ledger.conservation();
        assert_eq!(
            conservation.residual_ms, ledger.input_overflow_ms,
            "the overflow is the only scope difference in this sample, and it makes the named \
             parts sum to MORE than total_ms: {conservation:?}"
        );
        assert_eq!(
            conservation.residual_ms,
            raw_named
                + ledger.detector_preprocess_ms
                + ledger.detector_infer_ms
                + ledger.detector_postprocess_ms
                + ledger.recognizer_preprocess_ms
                + ledger.recognizer_infer_ms
                + ledger.recognizer_postprocess_ms
                + ledger.page_postprocess_ms
                - ledger.total_ms,
            "the raw named parts (including the overflow) must reconcile to total_ms"
        );
        assert!(
            !conservation.conserved,
            "a 7 ms scope difference must fail the conservation check"
        );
        assert_eq!(
            conservation.excess_ms, 0.0,
            "a positive residual reports excess_ms = 0.0 (the opposite direction)"
        );
        assert!(
            conservation
                .interpretation
                .contains("SCOPE difference in the opposite direction"),
            "the overflow must be read as a scope difference in the opposite direction: {}",
            conservation.interpretation
        );
        assert!(
            conservation
                .interpretation
                .contains("they are sequential (rapid_ocr.rs:619-719 then :106-130)"),
            "the overflow interpretation must still cite the sequential relationship: {}",
            conservation.interpretation
        );
        let shares = ledger.shares().expect("shares");
        assert!(
            (shares.rust_share * ledger.total_ms + shares.inference_share * ledger.total_ms
                - (ledger.attributed_ms() - ledger.input_overflow_ms))
                .abs()
                < 1e-9,
            "rust_ms + inference_ms must equal attributed_ms minus the input overflow, because \
             rust_ms uses the clipped input_ms while attributed_ms keeps the raw input parts"
        );
        assert!(
            (ledger.rust_ms() + ledger.inference_ms() - ledger.attributed_ms()
                + ledger.input_overflow_ms)
                .abs()
                < 1e-9,
            "the only difference between rust + inference and attributed must be the overflow"
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

    /// **守恒回归**：余量必须把差额暴露出来，而不是被悄悄吸收；两个方向都要显式报告。
    ///
    /// 这里命中“命名分量少于总额”的方向（本机实测的形态：内层 `inner.run()` 窗口里有一部分
    /// 墙钟不属于任何命名分量）→ 负残差 + `excess_ms > 0` + 口径差异解释。反方向
    /// （命名分量多于总额）由 `positive_residual_is_explained_as_a_different_direction` 覆盖。
    #[test]
    fn ledger_exposes_the_residual_instead_of_hiding_it() {
        let mut ledger = TimingLedger {
            total_ms: 100.0,
            detector_infer_ms: 90.0,
            ..TimingLedger::default()
        };
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        let conservation = ledger.conservation();
        assert_eq!(conservation.residual_ms, -10.0);
        assert_eq!(
            conservation.excess_ms, 10.0,
            "the residual magnitude is what the shares are valid to within"
        );
        assert!(
            !conservation.conserved,
            "a 10 ms difference must fail the conservation check"
        );

        // 把差额补上后必须恢复守恒，并且不再报告任何口径残差。
        ledger.input_decode_ms = 10.0;
        ledger.input_named_within_ms = 10.0;
        ledger.input_preprocess_ms = 10.0;
        ledger.unattributed_ms = ledger.total_ms - ledger.attributed_ms();
        assert_eq!(ledger.unattributed_ms, 0.0);
        let conserved = ledger.conservation();
        assert!(conserved.conserved);
        assert_eq!(conserved.excess_ms, 0.0);
        assert_eq!(ledger.input_overflow_ms, 0.0);
        assert!(ledger.input_ms() <= ledger.input_preprocess_ms);
        assert!(
            conserved.interpretation.contains("strict partition"),
            "a conserved ledger must say so: {}",
            conserved.interpretation
        );
    }

    /// 均值账本必须保持守恒：报告里的占比就是用它算的。
    #[test]
    fn mean_ledger_is_conserved() {
        let sample = |infer: f64, decode: f64| {
            let mut ledger = TimingLedger {
                total_ms: decode + infer,
                detector_infer_ms: infer,
                input_decode_ms: decode,
                input_preprocess_ms: decode,
                input_named_within_ms: decode,
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
        assert!((mean.input_named_within_ms - 50.0).abs() < 1e-9);
        assert!((mean.input_overflow_ms - 0.0).abs() < 1e-9);
        assert!((mean.input_ms() - 50.0).abs() < 1e-9);
        assert!(
            mean.input_ms() <= mean.input_preprocess_ms,
            "the mean ledger must keep the input invariant: {} <= {}",
            mean.input_ms(),
            mean.input_preprocess_ms
        );
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

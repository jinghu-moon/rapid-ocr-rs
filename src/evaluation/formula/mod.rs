//! 公式识别评测。
//!
//! 按领域拆分指标与 fixture；公式指标不得与普通 OCR 指标混合成一个默认汇总。
//! `HWE`（手写公式）作为附加测试单独报告，不并入印刷体平均值。

pub mod fixture;
pub mod metrics;

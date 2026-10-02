//! 共享评测框架。
//!
//! 按领域拆分：`evaluation::ocr` 承载普通 OCR 文本/检测指标；
//! `evaluation::formula` 承载公式识别指标。两类指标不得混合成一个默认汇总。

pub mod formula;
pub mod ocr;

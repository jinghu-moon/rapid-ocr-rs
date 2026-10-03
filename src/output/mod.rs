//! 共享输出层：把 OcrOutput 渲染为 JSON / Markdown / HTML 或可视化图像。
//!
//! 依赖方向：output 只依赖 pi（数据结构），不依赖 ocr / ormula 的实现，
//! 因此普通 OCR 与公式识别共用同一套输出，公式只是多了一种 RegionKind。
pub mod html;
pub mod json;
pub mod markdown;
pub mod visualize;

//! 普通 OCR bounded context。
//!
//! 依赖方向：`ocr` 只依赖共享层（`runtime`/`input`/`vision`/`output`/`error`/`config`/`types`），
//! 共享层不得反向依赖 `ocr`。检测、方向分类、CTC 识别与普通 OCR 编排全部收拢在本域。

pub mod cls;
pub mod det;
pub mod pipeline;
pub mod rec;

pub mod config;
pub mod types;

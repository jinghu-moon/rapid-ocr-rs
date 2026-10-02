//! 公式识别 bounded context。
//!
//! 依赖方向：`formula` 只依赖共享层（`runtime`/`input`/`vision`/`error` 等），
//! 不依赖 `ocr` 生产识别器；普通 OCR 也不得引用 `formula`。
//!
//! 阶段 0B 只建立空领域模块；阶段 3 增加模型契约探针 `model_info`。
//! `preprocess`/`tokenizer`/`session` 在阶段 4/5/6 实现。

pub mod model_info;
pub mod preprocess;
pub mod session;
pub mod tokenizer_metadata;

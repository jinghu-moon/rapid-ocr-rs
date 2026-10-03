//! 共享视觉层：纯 Rust 的 resize / rotate / quad crop 与线性重采样 scratch。
//!
//! 依赖方向：ision 不依赖 ocr / ormula，只提供与模型契约解耦的数值例程。
//! 这里**没有**后端分派：OpenCV 后端及其分派在 Windows x64 收窄中删除（无测量依据），
//! 数值一致性要求（与 OpenCV/Pillow 的行为对齐）改为写在各自的注释里并由测试守护。
pub(crate) mod image_backend;
pub(crate) mod resize;
pub(crate) mod rotate_crop;

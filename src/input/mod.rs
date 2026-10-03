//! 共享输入层：把各种 ImageInput 统一解码为可参与管线的图像。
//!
//! 依赖方向：input 不依赖 ocr / ormula。普通 OCR 与公式识别都调用这里，
//! 因此编码字节上限、header 像素探测、EXIF 方向、URL 超时与解码错误语义只有一份实现，
//! 两条 pipeline 不再各自维护解码逻辑。
pub mod image_loader;

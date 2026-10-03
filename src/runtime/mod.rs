//! 共享 runtime 层：ONNX Runtime 会话、provider 解析、运行时画像与进程内存采集。
//!
//! 依赖方向：`runtime` **不依赖** `ocr` / `formula`（两个 bounded context），只依赖
//! `config` / `error`。普通 OCR 与公式识别都通过这里创建会话，因此：
//!
//! - provider 语义（请求、解析、回退、严格拒绝）只有一份实现；
//! - 线程策略由 [`profile::RuntimeProfile`] 统一决定，两条 pipeline 不再各配一套；
//! - 峰值内存口径只有 Windows PSAPI 一种（[`memory`]）。
//!
//! 模块划分：
//!
//! - [`session`]：`OrtSession`，模型加载、契约探测与实际推理调用；
//! - [`contracts`]：模型输入/输出张量契约的类型与探测结果；
//! - [`provider`]：执行提供者解析与错误语义；
//! - [`ort_runtime`]：已加载 ONNX Runtime 运行库的指纹（版本/路径/体积/SHA-256/provider DLL）；
//! - [`timing`]：把 [`crate::api::OcrTimings`] 拆成显式命名项 + 显式余量的时间账本
//!   （**诊断**工具：分量来自互相重叠的计时窗口，占比只在残差量级内成立）；
//! - [`profile`]：provider + 线程 + arena + 公式批大小的统一画像；
//! - [`memory`]：进程峰值工作集采集（Windows x64）。

pub mod contracts;
pub mod memory;
pub mod ort_runtime;
pub mod profile;
pub mod provider;
pub mod session;
pub mod timing;

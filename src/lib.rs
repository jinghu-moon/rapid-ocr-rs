//! `rapid-ocr-rs`：用 Rust 调用 PP-OCR / PP-FormulaNet 系列 ONNX 模型。
//!
//! # 平台支持
//!
//! **仅支持 `x86_64-pc-windows-msvc`。**
//!
//! 这是刻意的收窄，而不是尚未移植：项目的实际运行环境只有 Windows，
//! 保留 Linux/macOS 分支会让平台差异散落到业务代码里，并让验证矩阵无法收敛。
//! 明确非目标：`aarch64-pc-windows-msvc`、`x86_64-pc-windows-gnu`、Wine、WSL、
//! Linux、macOS。若将来需要，另立平台计划，而不是在这里逐步加回 `cfg`。
//!
//! 平台边界集中在 crate 根部，规则只有三条：
//!
//! 1. 门槛写在 `src/platform_gate.rs`（唯一定义处，无任何依赖），可以用
//!    `tools/check_platform_gate.ps1` 单独对一个非 Windows target 验证“命中的是
//!    我们自己的错误信息”；
//! 2. 所有模块与重导出都带同一个 `#[cfg]` 谓词，因此非支持平台**只会**看到那一条
//!    `compile_error!`，不会退化成一堆“缺少某个 Windows API”的模糊诊断；
//! 3. 业务代码内部不再散布 `cfg(windows)`；平台差异只允许出现在
//!    `runtime/memory.rs` 这类平台实现模块里。

#[path = "platform_gate.rs"]
mod platform_gate;

#[cfg(all(windows, target_arch = "x86_64"))]
mod api;
#[cfg(all(windows, target_arch = "x86_64"))]
mod config;
#[cfg(all(windows, target_arch = "x86_64"))]
mod error;
#[cfg(all(windows, target_arch = "x86_64"))]
pub mod evaluation;
#[cfg(all(windows, target_arch = "x86_64"))]
mod exports;
#[cfg(all(windows, target_arch = "x86_64"))]
mod formula;
#[cfg(all(windows, target_arch = "x86_64"))]
mod input;
#[cfg(all(windows, target_arch = "x86_64"))]
mod model_registry;
#[cfg(all(windows, target_arch = "x86_64"))]
mod model_store;
#[cfg(all(windows, target_arch = "x86_64"))]
mod ocr;
#[cfg(all(windows, target_arch = "x86_64"))]
mod output;
#[cfg(all(windows, target_arch = "x86_64"))]
mod runtime;
#[cfg(all(windows, target_arch = "x86_64", test))]
mod test_support;
#[cfg(all(windows, target_arch = "x86_64"))]
mod vision;

/// 公开 API 的唯一入口（见 `src/exports.rs`）。
///
/// 平台谓词只在这一处出现，避免在十几个 `pub use` 上重复。
#[cfg(all(windows, target_arch = "x86_64"))]
pub use exports::*;

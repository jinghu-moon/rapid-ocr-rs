//! 平台门槛：本 crate 支持且仅支持的平台是 `x86_64-pc-windows-msvc`。
//!
//! 这个文件刻意**不依赖任何东西**，因此可以被独立编译来验证门槛本身：
//!
//! ```text
//! rustc --edition 2024 --crate-type lib --emit=metadata --target x86_64-linux-android src/platform_gate.rs
//! ```
//!
//! 预期只输出下面这条 `compile_error!`。`tools/check_platform_gate.ps1` 就是这个命令的
//! 可执行封装，用来证明“非 Windows 命中的是我们自己的错误”，而不是依赖项的模糊失败。
//!
//! 明确的非目标：`aarch64-pc-windows-msvc`、`x86_64-pc-windows-gnu`、Wine、WSL、
//! Linux、macOS。

#[cfg(not(all(windows, target_arch = "x86_64")))]
compile_error!(
    "rapid-ocr-rs only supports x86_64-pc-windows-msvc; other targets (Windows ARM64, GNU ABI, \
     Wine/WSL, Linux, macOS) are explicit non-goals. See the crate documentation for the platform \
     plan."
);

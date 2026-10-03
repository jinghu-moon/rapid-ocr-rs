//! 平台门槛：本 crate 支持且仅支持的平台是 **Windows x64 + MSVC ABI**
//! （`x86_64-pc-windows-msvc`）。
//!
//! 这个文件刻意**不依赖任何东西**，因此可以被独立编译来验证门槛本身：
//!
//! ```text
//! rustc --edition 2024 --crate-type lib --emit=metadata --target x86_64-linux-android src/platform_gate.rs
//! rustc --edition 2024 --crate-type lib --emit=metadata --target x86_64-pc-windows-gnu src/platform_gate.rs
//! ```
//!
//! 预期只输出下面这条 `compile_error!`。`tools/check_platform_gate.ps1` 就是这两个命令的
//! 可执行封装，用来证明“非支持目标命中的是我们自己的错误”，而不是依赖项的模糊失败。
//!
//! **谓词必须同时包含 `target_arch = "x86_64"` 与 `target_env = "msvc"`。**
//! 只写 `all(windows, target_arch = "x86_64")` 会把 `x86_64-pc-windows-gnu` 也放进来
//! （它同样是 Windows + x86_64），而本 crate 只在 MSVC ABI 上构建与验证：导入库名、
//! `#[link(name = "psapi")]` 之类的原生链接约定、以及 ort 预编译运行库都是 MSVC 产物。
//! GNU ABI 需要另一套链接配置，属于明确非目标。
//!
//! 明确的非目标：**Windows x86（32 位，`i686-pc-windows-msvc` / `i686-pc-windows-gnu`）**、
//! **Windows ARM64（`aarch64-pc-windows-msvc`）**、**Windows GNU ABI
//! （`x86_64-pc-windows-gnu`）**，以及 Wine、WSL、Linux、macOS。
//!
//! 32 位 x86 不是“改一个 `cfg` 就能支持”的目标：预编译的 ONNX Runtime、
//! DirectML/CUDA 的 provider DLL 与约 566 MB 的公式识别模型都是 x64 产物，
//! 32 位地址空间对后者本身就是真实限制。

#[cfg(not(all(windows, target_arch = "x86_64", target_env = "msvc")))]
compile_error!(
    "rapid-ocr-rs only supports x86_64-pc-windows-msvc (Windows x64 + MSVC ABI); other targets \
     (Windows x86/i686, Windows ARM64, Windows GNU ABI, Wine/WSL, Linux, macOS) are explicit \
     non-goals. See the crate documentation for the platform plan."
);

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
//! 平台边界集中在 crate 根部，规则只有四条：
//!
//! 1. 门槛写在 `src/platform_gate.rs`（唯一定义处，无任何依赖），可以用
//!    `tools/check_platform_gate.ps1` 单独对非支持 target 验证“命中的是
//!    我们自己的错误信息”；
//! 2. 所有模块与重导出都带**同一个** `#[cfg]` 谓词，因此非支持平台**只会**看到那一条
//!    `compile_error!`，不会退化成一堆“缺少某个 Windows API”的模糊诊断；
//! 3. 谓词是 `all(windows, target_arch = "x86_64", target_env = "msvc")`，**必须带
//!    `target_env = "msvc"`**：`x86_64-pc-windows-gnu` 同样满足
//!    `windows + x86_64`，但本 crate 的导入库、`#[link(name = "psapi")]` 与 ort
//!    预编译运行库都是 MSVC 产物，GNU ABI 属于明确非目标（见 `platform_gate.rs`）；
//! 4. 业务代码内部不再散布 `cfg(windows)`；平台差异只允许出现在
//!    `runtime/memory.rs` 这类平台实现模块里。
//!
//! 谓词重复出现，因此用 `cfg!` 断言把它锁住：`platform_gate_matches_the_module_predicate`
//! 测试会在**错误的 target**（例如 GNU）上编译测试套件时直接失败，而不是静默地
//! 用一套与门槛不一致的谓词继续构建。

#[path = "platform_gate.rs"]
mod platform_gate;

#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod api;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod config;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod error;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
pub mod evaluation;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod exports;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod formula;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod input;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod model_registry;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod model_store;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod ocr;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod output;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod runtime;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc", test))]
mod test_support;
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
mod vision;

/// 公开 API 的唯一入口（见 `src/exports.rs`）。
///
/// 平台谓词只在这一处出现，避免在十几个 `pub use` 上重复。
#[cfg(all(windows, target_arch = "x86_64", target_env = "msvc"))]
pub use exports::*;

#[cfg(test)]
mod platform_tests {
    /// 构建目标必须与 [`crate::platform_gate`] 允许的平台完全一致。
    ///
    /// `platform_gate.rs` 与 `lib.rs` 里的 `#[cfg]` 是同一个谓词的两份拷贝；如果只改对
    /// 一处，另一处会静默地把错误目标放进来（`x86_64-pc-windows-gnu` 就同时满足
    /// `windows` 与 `target_arch = "x86_64"`）。
    ///
    /// 这条 `const` 断言在**错误 target 上构建测试套件**时直接编译失败，因此不可能
    /// “跑过了但还是错的”：
    ///
    /// ```text
    /// error[E0080]: evaluation of constant value failed
    /// rapid-ocr-rs only supports x86_64-pc-windows-msvc ...
    /// ```
    const _: () = {
        assert!(
            cfg!(all(windows, target_arch = "x86_64", target_env = "msvc")),
            "rapid-ocr-rs only supports x86_64-pc-windows-msvc; this test suite is being built \
             for a different target"
        );
    };

    /// 上一条断言是编译期的；这里再把**实际**的 target 打进测试输出，
    /// 让 `cargo test` 的日志本身就能证明跑的是 Windows x86_64 MSVC。
    ///
    /// 这里刻意**不**再写 `cfg!(target_env = "msvc")` 断言：那是常量断言，clippy 的
    /// `assertions_on_constants` 会（正确地）报错，而且它已经被上面的 `const` 块覆盖。
    /// 这里断言的是运行期可见的三元组，两者互相独立。
    #[test]
    fn the_test_suite_runs_on_the_gated_target() {
        assert_eq!(std::env::consts::OS, "windows", "the crate is Windows-only");
        assert_eq!(std::env::consts::ARCH, "x86_64", "the crate is x86_64-only");
        assert_eq!(
            std::env::consts::FAMILY,
            "windows",
            "the crate targets the Windows family"
        );
        eprintln!(
            "target: os={} arch={} family={} (the compile-time predicate also requires \
             target_env = \"msvc\")",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        );
    }
}

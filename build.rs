//! 构建期信息：把 ort-sys 实际链接进来的 ONNX Runtime **静态库**记录下来。
//!
//! # 为什么需要它
//!
//! 本 crate 用默认 feature 构建时，`ort-sys` 采取的是 `rustc-link-lib=static=onnxruntime`
//! （见 `target/<profile>/build/ort-sys-*/output`）：**ONNX Runtime 被静态链接进可执行
//! 文件**，进程里根本没有 `onnxruntime.dll` 模块。此时
//! `GetModuleHandleW("onnxruntime.dll")` 必然失败，报告也就无法通过“运行库 DLL”来定位
//! 到底是哪一份 ORT。
//!
//! 因此这里把“链接进来的那份静态库”写进编译期环境变量：
//!
//! - [`LINK_LIB_BASE_ENV`]：ort 缓存目录（`.../ort.pyke.io/dfbin/<target>/`），
//!   每个分发版本一个内容哈希子目录；
//! - [`LINK_LIB_NAME_ENV`]：静态库文件名（`onnxruntime.lib`）；
//! - [`LINK_LIB_SIZE_ENV`]：该次链接真正使用的那份 `onnxruntime.lib` 的字节数。
//!
//! 运行期用“目录 + 文件名 + 体积”在缓存里唯一定位那份库文件并计算 SHA-256：两个已知
//! 分发哈希目录里的 `onnxruntime.lib` 体积不同（`341,152,186` 与 `363,923,500` 字节），
//! 因此体积本身就能把候选区分开，哈希则用来复核。找不到时报告给出可定位原因，
//! **不编造哈希**。
//!
//! # 信息从哪里来
//!
//! 两条来源，按可靠性排序：
//!
//! 1. `DEP_ORT_SYS_LINK`：cargo 为 `links = "..."` 的依赖准备的元数据变量。`ort-sys`
//!    目前没有声明 `links`，所以这条通常不存在；保留它是为了将来上游补上时自动生效。
//! 2. **扫描 `target/<profile>/build/` 下 `ort-sys-*` 的输出文件**（以及 `output`），
//!    找 `cargo:rustc-link-search=native=<dir>` 与随后出现的 `static=<name>`。
//!    这是构建期事实的直接证据，也正是 `cargo build -vv` 会打印的那两行。

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(raw) = capture_from_env().or_else(|| capture_from_build_dir(&out_dir)) else {
        // 没有链接信息（例如换成 load-dynamic、或依赖的构建脚本换了输出格式）：
        // 不设置任何变量，运行期会报告“没有链接信息”这一可定位原因。
        return;
    };

    let Some((base_dir, file_name)) = split_base_dir(&raw) else {
        return;
    };
    let Ok(metadata) = std::fs::metadata(&raw) else {
        return;
    };

    println!("cargo:rustc-env={LINK_LIB_BASE_ENV}={}", base_dir.display());
    println!("cargo:rustc-env={LINK_LIB_NAME_ENV}={file_name}");
    println!("cargo:rustc-env={LINK_LIB_SIZE_ENV}={}", metadata.len());
}

/// 编译期环境变量：ort 缓存里分发目录的公共父目录。
pub const LINK_LIB_BASE_ENV: &str = "RAPID_OCR_ORT_LINK_DIR";
/// 编译期环境变量：被链接的静态库文件名（通常是 `onnxruntime.lib`）。
pub const LINK_LIB_NAME_ENV: &str = "RAPID_OCR_ORT_LINK_NAME";
/// 编译期环境变量：被链接的静态库字节数。
pub const LINK_LIB_SIZE_ENV: &str = "RAPID_OCR_ORT_LINK_BYTES";

/// 优先使用 cargo 提供的依赖元数据变量（`ort-sys` 将来声明 `links` 时可用）。
fn capture_from_env() -> Option<PathBuf> {
    let raw = std::env::var_os("DEP_ORT_SYS_LINK")
        .or_else(|| std::env::var_os("ORT_SYS_LINK"))
        .or_else(|| std::env::var_os("ORT_LIB_LOCATION"))?;
    let candidate = PathBuf::from(raw);
    candidate.exists().then_some(candidate)
}

/// 扫描构建目录里 `ort-sys-*` 的输出，提取 `rustc-link-search` + `static=<name>`。
fn capture_from_build_dir(out_dir: &Path) -> Option<PathBuf> {
    // `OUT_DIR` = `<target>/<profile>/build/<our-crate>-<hash>/out`；
    // 依赖的构建输出都在它的上两级。
    let build_dir = out_dir.parent()?.parent()?;
    let entries = std::fs::read_dir(build_dir).ok()?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ort-sys-"))
        })
        .collect();
    // 目录名带内容哈希，排序只是为了结果稳定。
    dirs.sort();

    let mut parts: Vec<(PathBuf, String)> = Vec::new();
    for dir in dirs {
        for file in ["output", "stderr", "stdout"] {
            let Ok(text) = std::fs::read_to_string(dir.join(file)) else {
                continue;
            };
            if let Some(found) = parse_link_lines(&text) {
                parts.push(found);
            }
        }
    }
    // 同一份分发可能出现在多个构建目录里；只要任意一个文件真实存在就可信。
    parts.into_iter().find_map(|(dir, name)| {
        let candidate = dir.join(&name);
        candidate.exists().then_some(candidate)
    })
}

/// 从构建脚本输出里解析出被静态链接的库文件路径。
///
/// ort-sys 会打印成对的两行：
///
/// ```text
/// cargo:rustc-link-search=native=<dir>
/// cargo:rustc-link-lib=static=onnxruntime
/// ```
fn parse_link_lines(text: &str) -> Option<(PathBuf, String)> {
    let mut search_dir: Option<PathBuf> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(dir) = line.strip_prefix("cargo:rustc-link-search=native=") {
            search_dir = Some(PathBuf::from(dir));
            continue;
        }
        if let Some(name) = line.strip_prefix("cargo:rustc-link-lib=static=") {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            // MSVC 的静态库文件名等于 `name.lib`；已带扩展名时不重复追加。
            let file_name = if name.ends_with(".lib") {
                name.to_string()
            } else {
                format!("{name}.lib")
            };
            if let Some(dir) = search_dir.take() {
                return Some((dir, file_name));
            }
        }
    }
    None
}

/// 把 `<base>/<dist-hash>/<file>` 拆成 `(<base>, <file>)`。
///
/// `base` 只保留到 `dfbin/<target>` 这一层，运行期再去枚举它下面的分发目录。
fn split_base_dir(resolved: &Path) -> Option<(PathBuf, String)> {
    let file_name = resolved.file_name()?.to_string_lossy().to_string();
    let dist_dir = resolved.parent()?;
    let base_dir = dist_dir.parent()?.to_path_buf();
    Some((base_dir, file_name))
}

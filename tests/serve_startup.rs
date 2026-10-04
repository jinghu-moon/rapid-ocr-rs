//! `rapidocr serve` 的**进程边界**验收（A1：`--reverify-models` 的启动期 fail-fast）。
//!
//! 只有启用 `serve` feature 时才有意义：默认构建里 `rapidocr serve` 是一句可定位的
//! 拒绝（§2.1：HTTP 层是 optional feature），那条路径不是本文件要考察的东西。
//! 因此整个文件在未启用 feature 时被 `cfg` 掉（`Cargo.toml` 的 `[[test]]` 也声明了
//! `required-features`，两道保险）。
//!
//! # 为什么它在 `tests/` 而不是在 `src/bin/serve/tests.rs`
//!
//! 单元测试跑在 `target/<profile>/deps/rapidocr-*.exe` 里，cargo 在那里**不**保证注入
//! `CARGO_BIN_EXE_rapidocr`（`src/bin` 的 `#[cfg(test)] mod tests` 只能靠猜上一级目录，
//! 猜到的可能是 `cargo build` 留下的、**没有** `serve` feature 的旧可执行文件——
//! 那会让测试断言一个与本次构建无关的东西）。集成测试里 cargo **保证**注入这个变量，
//! 它指向本次 `cargo test --features serve` 构建出来的 `rapidocr.exe`。
//!
//! # 这一条用例证什么
//!
//! 真实的命令行 + 真实的模型目录 + 真实的进程退出码：
//!
//! 1. 缺失的模型文件 → 进程在**验证阶段**（建会话之前）以非零状态退出；
//! 2. 错误里点名那个文件**和**它的状态（`missing`），并说明是 `--reverify-models` 拦下的；
//! 3. 启动日志里那个文件已经有一行逐文件结论（说明日志先于失败写出）。
//!
//! 这个用例不加载任何 ONNX：失败发生在运行库被使用之前。

#![cfg(feature = "serve")]

use std::path::{Path, PathBuf};

/// 一个本地清单描述的模型目录（文本三个 role + 公式识别 role，M4 起每个清单都要有）。
///
/// 与 `src/bin/serve/tests.rs` 的夹具同形状：这里刻意**不复用**私有测试模块
/// （集成测试看不到 `#[cfg(test)]` 下的东西），也刻意不依赖 `target/` 下的任何旧产物。
fn manifest_model_dir(root: &Path, name: &str) -> PathBuf {
    manifest_model_dir_with(root, name, false)
}

/// 与 [`manifest_model_dir`] 相同，`with_detector` 时再声明一个 `formula_detector` role
/// （`mfd.onnx`）——集合声明它就意味着"公式管线属于这次运行"。
fn manifest_model_dir_with(root: &Path, name: &str, with_detector: bool) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let mut files: Vec<(&str, &str)> = vec![
        ("det.onnx", "detector"),
        ("rec.onnx", "recognizer"),
        ("dict.txt", "dictionary"),
        ("fx.onnx", "formula_recognizer"),
    ];
    if with_detector {
        files.push(("mfd.onnx", "formula_detector"));
    }
    let mut manifest = String::from(
        "{\"schema_version\":1,\"id\":\"startup-set\",\"family\":\"PP-OCR\",\
         \"version\":\"v-test\",\"files\":[",
    );
    for (index, (file, role)) in files.iter().enumerate() {
        let path = dir.join(file);
        let body: Vec<u8> = if *file == "mfd.onnx" {
            // 没有声明摘要的规则不适用（清单会写下**真实**摘要），但内容仍然写成
            // "像 ONNX"的样子，避免夹具本身成为结论的来源。
            vec![0x08, 0x07, b'm', b'f', b'd']
        } else {
            format!("startup fixture {file}").into_bytes()
        };
        std::fs::write(&path, &body).expect("write the model file");
        let sha = sha256_file(&path);
        if index > 0 {
            manifest.push(',');
        }
        manifest.push_str(&format!(
            "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\"}}"
        ));
    }
    manifest.push_str("]}");
    std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    dir
}

/// 测试用的 SHA-256（只读几十字节的夹具；不引入任何库依赖）。
fn sha256_file(path: &Path) -> String {
    // `rapid_ocr_rs` 的下载/校验路径用的就是它，这里直接用同一个实现，
    // 避免在集成测试里手写第二套哈希。
    rapid_ocr_rs::sha256_file(path).expect("hash the fixture")
}

/// A1：损坏/缺失的计划模型 → `rapidocr serve --reverify-models` **拒绝启动**。
#[test]
fn the_serve_command_refuses_to_start_when_the_plan_model_is_missing() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_rapidocr"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-serve-tests");
    std::fs::create_dir_all(&root).expect("create the fixture root");
    let dir = manifest_model_dir(&root, "cli-reverify-process");
    // 缺失的模型文件：启动期验证必须在建会话之前就拒绝。
    std::fs::remove_file(dir.join("rec.onnx")).expect("remove the recognizer");

    // 不给 `--config`：用库内建默认（`det`/`rec` 同 model_type）。这个用例考察的是
    // **模型验证**这一步，给配置只会把失败点提前到配置校验上（那是另一条路径）。
    let output = std::process::Command::new(&binary)
        .args([
            "serve",
            "--model-dir",
            &dir.display().to_string(),
            "--reverify-models",
            "--port",
            "0",
        ])
        .output()
        .expect("the serve command must be runnable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a run whose plan model is missing must refuse to start\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("rec.onnx") && stderr.contains("missing"),
        "the locating error must name the offending file **and** its state\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--reverify-models"),
        "the error must say which switch caused the refusal\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("--reverify-models rec.onnx") && stdout.contains("missing"),
        "the startup log must record the per-file conclusion before failing\nstdout:\n{stdout}"
    );
}

/// 本轮（需求 1 的进程边界那一半）：**公式检测模型**损坏（内容与集合声明的摘要不符）时
/// `--reverify-models` 同样拒绝启动，错误里点名那个文件**和它所属的管线**。
///
/// 旧实现的根因：`ModelPlan::required_files` 只收 detector/recognizer/dictionary +
/// `formula_recognizer`，把 `formula_detector` 漏在外面——`--reverify-models` 因此从不
/// 冷验证一个 `--formula-detector`（或集合声明的检测模型），一个"存在但内容错"的检测模型
/// 可以一路留到第一次公式请求。
#[test]
fn the_serve_command_refuses_to_start_when_the_formula_detector_is_corrupt() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_rapidocr"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-serve-tests");
    std::fs::create_dir_all(&root).expect("create the fixture root");
    // 集合声明了 `formula_detector` → 公式管线属于这次运行（不需要 --formula-detector）。
    let dir = manifest_model_dir_with(&root, "cli-reverify-detector", true);
    std::fs::write(dir.join("mfd.onnx"), b"corrupted detector bytes")
        .expect("corrupt the detector");

    let output = std::process::Command::new(&binary)
        .args([
            "serve",
            "--model-dir",
            &dir.display().to_string(),
            "--reverify-models",
            "--port",
            "0",
        ])
        .output()
        .expect("the serve command must be runnable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a run whose formula detector is corrupt must refuse to start\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("mfd.onnx") && stderr.contains("corrupt"),
        "the locating error must name the detector and its state\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("formula pipeline"),
        "the error must say which pipeline the file belongs to\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--reverify-models"),
        "the error must say which switch caused the refusal\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("--reverify-models mfd.onnx")
            && stdout.contains("digest computed by this call: true"),
        "the startup log must record the detector's cold conclusion before failing\nstdout:\n\
         {stdout}"
    );
}

//! 测试用的仓库外资产定位。
//!
//! 模型权重和公开测试集体积很大，不随 crate 提交，只能通过环境变量引用：
//!
//! - `RAPID_OCR_MODEL_ROOT`：模型根目录（例如 `<workspace>/OCR-Model`），也可以直接
//!   指向 `pp_formulanet_plus_m.onnx`；
//! - `RAPID_OCR_FORMULA_MODEL`：直接指向公式 ONNX，优先于根目录；
//! - `RAPID_OCR_FORMULA_TEST_ROOT`：公式测试集根目录（例如 `<workspace>/Formula-TestSet`）。
//!
//! 缺失资产时测试必须 **skip**，不允许 panic，也不允许回落到开发机绝对路径，
//! 否则干净 clone / CI 无法复现测试结论。需要在缺少资产时失败的流水线可以设置
//! `RAPID_OCR_REQUIRE_EXTERNAL_ASSETS=1`，此时缺失资产会 panic 并说明原因。

use std::path::{Path, PathBuf};

/// 公式模型在模型根目录下的标准相对路径。
pub const FORMULA_MODEL_RELATIVE: &str =
    "Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx";

fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn require_external_assets() -> bool {
    std::env::var("RAPID_OCR_REQUIRE_EXTERNAL_ASSETS")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// 返回可用资产，或在缺失时跳过/失败。
///
/// 返回值语义：`Some` 表示资产可用；`None` 表示当前环境缺少该资产，调用方必须
/// 直接 `return` 跳过测试。
pub fn asset(description: &str, value: Option<PathBuf>) -> Option<PathBuf> {
    match value {
        Some(path) => Some(path),
        None => {
            if require_external_assets() {
                panic!(
                    "missing external test asset: {description}; set the documented \
                     RAPID_OCR_MODEL_ROOT / RAPID_OCR_FORMULA_TEST_ROOT environment variables"
                );
            }
            eprintln!("skipping test: {description} is not available in this environment");
            None
        }
    }
}

/// 仓库内 fixture 目录，随仓库提交，任何环境都必须可用。
pub fn fixture_dir(kind: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(kind)
}

/// PP-FormulaNet_plus-M ONNX 路径；缺失时返回 `None`。
pub fn formula_model_path() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_MODEL")
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            if root.is_file() {
                return Some(root);
            }
            let direct = root.join(FORMULA_MODEL_RELATIVE);
            direct.is_file().then_some(direct)
        });
    asset(
        "PP-FormulaNet_plus-M onnx (set RAPID_OCR_MODEL_ROOT or RAPID_OCR_FORMULA_MODEL)",
        candidate,
    )
}

/// `pix2text-mfd-1.5.onnx` 页面公式检测模型路径；缺失时返回 `None`。
pub fn formula_detector_path() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_DETECT_MODEL")
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            let direct = root.join("Formula-Detection-Model/pix2text-mfd-1.5.onnx");
            direct.is_file().then_some(direct)
        });
    asset(
        "pix2text-mfd-1.5 onnx (set RAPID_OCR_MODEL_ROOT or RAPID_OCR_FORMULA_DETECT_MODEL)",
        candidate,
    )
}

/// 普通 OCR 模型根目录（`OCR-Model`）；缺失时返回 `None`。
pub fn ocr_model_root() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_MODEL_ROOT").filter(|path| path.is_dir());
    asset("OCR model root (set RAPID_OCR_MODEL_ROOT)", candidate)
}

/// 页面级集成测试用的真实页面图片。
///
/// 位置从模型根目录的**同级目录**推导（`<workspace>/OCR-test-image`），也可以通过
/// `RAPID_OCR_TEST_IMAGES` 覆盖；两者都不可用时返回 `None` 并跳过测试。
pub fn page_fixture(name: &str) -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_TEST_IMAGES")
        .map(|root| root.join(name))
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            let root = root.parent()?.join("OCR-test-image").join(name);
            root.is_file().then_some(root)
        });
    asset(
        &format!("page fixture `{name}` (set RAPID_OCR_TEST_IMAGES or RAPID_OCR_MODEL_ROOT)"),
        candidate,
    )
}

/// 公式测试集根目录；缺失时返回 `None`。
pub fn formula_dataset_root() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_TEST_ROOT").filter(|path| path.is_dir());
    asset(
        "formula test set (set RAPID_OCR_FORMULA_TEST_ROOT)",
        candidate,
    )
}

/// 单元测试用的临时目录：进程内唯一，`Drop` 时删除。
///
/// 模型清单的逐文件状态、来源选择都需要真实文件系统行为（存在 / 缺失 / 哈希不匹配），
/// 因此这些测试用同一个小工具建目录，而不是各自实现一遍临时目录命名与清理。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir should be creatable");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.path.join(name), contents).expect("fixture should be writable");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

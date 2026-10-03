//! 公式识别测试 fixture 读取层。
//!
//! 本模块只负责把仓库外的本地测试集解析为统一的 [`FormulaSample`]，
//! 供阶段 9 的数值回归使用。它不依赖 `ocr` 或 `formula` 生产识别器，
//! 也不做任何模型推理。
//!
//! 数据事实（来自 `Formula-TestSet`）：
//! - `im2latex_formulas.norm.lst` 中存在空标签行；`im2latex_test_filter.lst` 有 71 条索引引用
//!   空标签。这些样本仍然“可定位”（图片存在），但 `ground_truth` 为空，必须用
//!   [`FormulaSample::has_ground_truth`] / [`FormulaFixture::scorable`] 排除出精确匹配统计，
//!   而不是静默丢弃（否则丢失评测目标）或让 loader 失败（否则无法覆盖完整 10,355 测试集）。
//! - `UniMER-Test` 的 `spe/sce` 图片文件名数字是原始 label 行索引，不能 `zip(sorted(images), labels)`。

use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::{RapidOcrError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormulaDataset {
    Im2Latex,
    LatexOcrExample,
    UniMer(UniMerSubset),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UniMerSubset {
    Spe,
    Cpe,
    Sce,
    Hwe,
}

impl UniMerSubset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spe => "spe",
            Self::Cpe => "cpe",
            Self::Sce => "sce",
            Self::Hwe => "hwe",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormulaSplit {
    Train,
    Test,
    Validate,
}

impl FormulaSplit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Test => "test",
            Self::Validate => "validate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaSample {
    /// 数据集内的相对路径，`/` 分隔。
    ///
    /// 抽样键与 manifest 都基于它，而不是绝对路径：把数据集复制到别的根目录
    /// （或换一台机器）不会改变样本选择与 manifest 哈希。
    ///
    /// 私有字段：只能由 [`FormulaSample::new`]（内部经 [`dataset_relative_path`]
    /// 校验）产生，从而保证它**永远不是绝对路径**。
    relative_path: String,
    pub image_path: PathBuf,
    pub ground_truth: String,
    pub dataset: FormulaDataset,
    pub split: FormulaSplit,
    /// 标签在原始标签文件中的行索引（0 基）。
    pub source_index: Option<usize>,
}

impl FormulaSample {
    /// 构造样本。`relative_path` 由调用方通过 [`dataset_relative_path`] 计算。
    ///
    /// 这里做的是**词法**校验（双保险）：空值、绝对路径、以及任何 `.`/`..`
    /// 或根/盘符成分都会被拒绝，因此字段本身不可能含路径穿越成分。
    pub fn new(
        relative_path: String,
        image_path: PathBuf,
        ground_truth: String,
        dataset: FormulaDataset,
        split: FormulaSplit,
        source_index: Option<usize>,
    ) -> Result<Self> {
        if relative_path.is_empty() {
            return Err(err_format(
                "formula sample relative path must not be empty".to_string(),
            ));
        }
        if Path::new(&relative_path).is_absolute() || relative_path.starts_with('/') {
            return Err(err_format(format!(
                "formula sample relative path `{relative_path}` must be relative to the dataset \
                 root, not an absolute path"
            )));
        }
        for component in Path::new(&relative_path).components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {
                    return Err(err_format(format!(
                        "formula sample relative path `{relative_path}` must not contain `.` \
                         components"
                    )));
                }
                _ => {
                    return Err(err_format(format!(
                        "formula sample relative path `{relative_path}` must not contain `..` or \
                         root components"
                    )));
                }
            }
        }
        Ok(Self {
            relative_path,
            image_path,
            ground_truth,
            dataset,
            split,
            source_index,
        })
    }

    /// 数据集内的相对路径（`/` 分隔，永不为绝对路径）。
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    /// 是否具有非空真值。空真值样本不做精确匹配统计（见模块文档）。
    pub fn has_ground_truth(&self) -> bool {
        !self.ground_truth.trim().is_empty()
    }
}

/// 计算数据集内的相对路径（`/` 分隔）。
///
/// 抽样与 manifest 都依赖它，因此结果必须**只含数据集内的普通路径成分**：
///
/// - 数据集清单里的绝对路径、`..` 穿越、以及指向根目录之外的符号链接都会让
///   “相对路径”带上机器相关或越界的成分，跨机器复现随之失效，因此一律拒绝；
/// - 因此这里**始终先 `canonicalize()`** 再比较，而不是先做词法前缀判断：
///   词法判断会接受 `<root>/../outside/image.png`（前缀匹配成功，剩下
///   `../outside/image.png`），也会接受穿过符号链接逃出根目录的路径。
///   规范化同时解决 Windows 上的大小写、短名（8.3）与 `.`/`..` 问题。
///
/// 规范化后的相对路径只由普通成分组成，用 `/` 连接返回。
pub fn dataset_relative_path(root: &Path, path: &Path) -> Result<String> {
    let canonical_root = root.canonicalize().map_err(|error| {
        err_format(format!(
            "dataset root {} is not accessible: {error}",
            root.display()
        ))
    })?;
    let canonical_path = path.canonicalize().map_err(|error| {
        err_format(format!(
            "formula image {} is not accessible: {error}",
            path.display()
        ))
    })?;
    let relative = canonical_path.strip_prefix(&canonical_root).map_err(|_| {
        err_format(format!(
            "formula image {} is outside the dataset root {}; a formula dataset must be \
                 self-contained (no absolute paths, `..` escapes, or symlinks leading outside) \
                 so that sample selection is reproducible across machines",
            canonical_path.display(),
            canonical_root.display()
        ))
    })?;

    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().replace('\\', "/")),
            // 规范化后的路径不应再出现这些成分；出现即说明假设被打破，直接拒绝。
            _ => {
                return Err(err_format(format!(
                    "formula image {} resolved to a non-normal path component",
                    canonical_path.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(err_format(format!(
            "formula image {} resolves to the dataset root itself",
            canonical_path.display()
        )));
    }
    Ok(parts.join("/"))
}

#[derive(Debug, Clone)]
pub struct FormulaFixture {
    pub dataset: FormulaDataset,
    pub split: FormulaSplit,
    pub root: PathBuf,
    pub samples: Vec<FormulaSample>,
}

impl FormulaFixture {
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 仅用于 smoke 抽样的子集，按顺序截取前 `count` 条。
    pub fn smoke(&self, count: usize) -> FormulaFixture {
        FormulaFixture {
            dataset: self.dataset,
            split: self.split,
            root: self.root.clone(),
            samples: self.samples.iter().take(count).cloned().collect(),
        }
    }

    /// 仅迭代具有非空真值的样本，供精确匹配统计使用。
    pub fn scorable(&self) -> impl Iterator<Item = &FormulaSample> {
        self.samples.iter().filter(|s| s.has_ground_truth())
    }

    /// 空真值样本（仍可定位但不参与精确匹配）。
    pub fn empty_ground_truth(&self) -> impl Iterator<Item = &FormulaSample> {
        self.samples.iter().filter(|s| !s.has_ground_truth())
    }
}

fn err_missing_file(path: &Path) -> RapidOcrError {
    RapidOcrError::FileNotFound(path.to_path_buf())
}

fn err_format(msg: impl Into<String>) -> RapidOcrError {
    RapidOcrError::Config(msg.into())
}

fn require_dir(path: &Path, label: &str) -> Result<()> {
    if !path.exists() {
        return Err(err_missing_file(path));
    }
    if !path.is_dir() {
        return Err(err_format(format!(
            "expected {label} directory, got non-directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_file(path: &Path, label: &str) -> Result<()> {
    if !path.exists() {
        return Err(err_missing_file(path));
    }
    if !path.is_file() {
        return Err(err_format(format!(
            "expected {label} file, got non-file: {}",
            path.display()
        )));
    }
    Ok(())
}

/// 解析 im2latex filter 一行：`<image_name> <formula_index>`。
///
/// 图片名可能含空格（测试集为 ASCII，但为稳健起见从右向左按最后一个空白拆分：
/// 最后一个空白后的 token 是索引，其余为图片名）。
fn parse_im2latex_line(line: &str) -> Result<(String, usize)> {
    let line = line.trim();
    let mut it = line.rsplitn(2, char::is_whitespace);
    let index_token = it
        .next()
        .ok_or_else(|| err_format("im2latex filter line is empty"))?;
    let name_token = it
        .next()
        .ok_or_else(|| err_format("im2latex filter line missing image name"))?
        .trim();
    if name_token.is_empty() {
        return Err(err_format("im2latex filter line missing image name"));
    }
    let formula_index = index_token.parse::<usize>().map_err(|_| {
        err_format(format!(
            "im2latex filter line has invalid formula index: {index_token}"
        ))
    })?;
    Ok((name_token.to_string(), formula_index))
}

/// Error when a dataset root does not exist; returns a structured error, not a panic.
pub fn load_im2latex(root: &Path, split: FormulaSplit) -> Result<FormulaFixture> {
    require_dir(root, "im2latex-100k root")?;
    let (filter_name, split): (&str, FormulaSplit) = match split {
        FormulaSplit::Test => ("im2latex_test_filter.lst", FormulaSplit::Test),
        FormulaSplit::Validate => ("im2latex_validate_filter.lst", FormulaSplit::Validate),
        _ => {
            return Err(err_format(
                "im2latex only provides `test` and `validate` splits",
            ));
        }
    };
    let labels_path = root.join("im2latex_formulas.norm.lst");
    let filter_path = root.join(filter_name);
    require_file(&labels_path, "im2latex formulas")?;
    require_file(&filter_path, "im2latex filter")?;

    // 保持原始行（含空行），因为 filter 的 formula_index 是原始行号。
    let labels = fs::read_to_string(&labels_path)?;
    let label_lines: Vec<&str> = labels.split('\n').collect();

    let filter_raw = fs::read_to_string(&filter_path)?;
    let mut samples = Vec::new();
    for (line_index, line) in filter_raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (image_name, formula_index) = parse_im2latex_line(line)
            .map_err(|e| err_format(format!("im2latex filter line {}: {e}", line_index + 1)))?;
        let image_path = root.join(&image_name);
        if !image_path.is_file() {
            return Err(err_missing_file(&image_path));
        }
        if formula_index >= label_lines.len() {
            return Err(err_format(format!(
                "im2latex formula index {formula_index} out of bounds ({} label lines)",
                label_lines.len()
            )));
        }
        let ground_truth = label_lines[formula_index]
            .trim_end_matches('\r')
            .to_string();
        samples.push(FormulaSample::new(
            dataset_relative_path(root, &image_path)?,
            image_path,
            ground_truth,
            FormulaDataset::Im2Latex,
            split,
            Some(formula_index),
        )?);
    }
    Ok(FormulaFixture {
        dataset: FormulaDataset::Im2Latex,
        split,
        root: root.to_path_buf(),
        samples,
    })
}

/// PaddleX 示例集：`val.txt`/`train.txt` 为 `<image_path>\t<latex>`（tab 分隔）。
pub fn load_latex_ocr_example(root: &Path, split: FormulaSplit) -> Result<FormulaFixture> {
    require_dir(root, "latexocr-example root")?;
    let images = root.join("images");
    require_dir(&images, "latexocr-example images")?;
    let list_name = match split {
        FormulaSplit::Train => "train.txt",
        FormulaSplit::Validate => "val.txt",
        _ => {
            return Err(err_format(
                "latexocr-example only provides `train` and `validate` (val.txt) splits",
            ));
        }
    };
    let list_path = root.join(list_name);
    require_file(&list_path, "latexocr-example list")?;

    let raw = fs::read_to_string(&list_path)?;
    let mut samples = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let rel = parts.next().ok_or_else(|| {
            err_format(format!(
                "val.txt line {} missing image path",
                line_index + 1
            ))
        })?;
        let latex = parts
            .next()
            .ok_or_else(|| err_format(format!("val.txt line {} missing latex", line_index + 1)))?;
        let rel = rel.trim();
        let image_path = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            root.join(rel)
        };
        if !image_path.is_file() {
            return Err(err_missing_file(&image_path));
        }
        samples.push(FormulaSample::new(
            dataset_relative_path(root, &image_path)?,
            image_path,
            latex.trim().to_string(),
            FormulaDataset::LatexOcrExample,
            split,
            Some(line_index),
        )?);
    }
    Ok(FormulaFixture {
        dataset: FormulaDataset::LatexOcrExample,
        split,
        root: root.to_path_buf(),
        samples,
    })
}

/// UniMER-Test。
///
/// `cpe`/`hwe` 的图片文件名数字对应 label 行索引（`zip(images, labels)` 可用）。
/// `spe`/`sce` 的图片文件名数字是原始 label 行的索引，不能 `zip(sorted(images), labels)`，
/// 必须按文件名索引到 label 行。label 文件保持原始行（含空行），因文件名索引是原始行号。
pub fn load_unimer(root: &Path, subset: UniMerSubset) -> Result<FormulaFixture> {
    require_dir(root, "UniMER-Test root")?;
    let sub_dir = root.join(subset.as_str());
    require_dir(&sub_dir, "UniMER subset dir")?;
    let labels_path = root.join(format!("{}.txt", subset.as_str()));
    require_file(&labels_path, "UniMER labels")?;

    let labels = fs::read_to_string(&labels_path)?;
    let label_lines: Vec<&str> = labels.split('\n').collect();

    let mut entries: Vec<(u32, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&sub_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| err_format(format!("unexpected image name: {}", path.display())))?;
        let index: u32 = stem
            .parse()
            .map_err(|_| err_format(format!("cannot parse UniMER image index: {stem}")))?;
        entries.push((index, path));
    }
    entries.sort_by_key(|(index, _)| *index);

    let mut samples = Vec::with_capacity(entries.len());
    for (index, image_path) in entries {
        let idx = index as usize;
        if idx >= label_lines.len() {
            return Err(err_format(format!(
                "UniMER {} image index {index} out of bounds ({} label lines)",
                subset.as_str(),
                label_lines.len()
            )));
        }
        let ground_truth = label_lines[idx].trim_end_matches('\r').to_string();
        samples.push(FormulaSample::new(
            dataset_relative_path(root, &image_path)?,
            image_path,
            ground_truth,
            FormulaDataset::UniMer(subset),
            FormulaSplit::Test,
            Some(idx),
        )?);
    }

    Ok(FormulaFixture {
        dataset: FormulaDataset::UniMer(subset),
        split: FormulaSplit::Test,
        root: root.to_path_buf(),
        samples,
    })
}

/// 校验样本图片可被解码（用于数据集完整性核验，不进入评测管线）。
pub fn check_images_decodable(fixture: &FormulaFixture) -> Result<()> {
    for sample in &fixture.samples {
        if !sample.image_path.is_file() {
            return Err(err_missing_file(&sample.image_path));
        }
        let bytes = fs::read(&sample.image_path)?;
        if image::load_from_memory(&bytes).is_err() {
            return Err(err_format(format!(
                "image not decodable: {}",
                sample.image_path.display()
            )));
        }
    }
    Ok(())
}

/// 按数据集分组统计每组序号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnimerSummary {
    pub spe_count: usize,
    pub cpe_count: usize,
    pub sce_count: usize,
    pub hwe_count: usize,
}

pub fn summarize_unimer(
    spe: &FormulaFixture,
    cpe: &FormulaFixture,
    sce: &FormulaFixture,
    hwe: &FormulaFixture,
) -> UnimerSummary {
    UnimerSummary {
        spe_count: spe.len(),
        cpe_count: cpe.len(),
        sce_count: sce.len(),
        hwe_count: hwe.len(),
    }
}

/// 检测 test 与 validate 样本是否有交集（按图片路径）。
pub fn overlap_count(a: &FormulaFixture, b: &FormulaFixture) -> usize {
    let set: BTreeSet<PathBuf> = a.samples.iter().map(|s| s.image_path.clone()).collect();
    b.samples
        .iter()
        .filter(|s| set.contains(&s.image_path))
        .count()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const TEST_ROOT_ENV: &str = "RAPID_OCR_FORMULA_TEST_ROOT";

    /// 公开测试集不随 crate 提交；缺失时必须 skip，不允许 panic 或回落到绝对路径。
    fn launcher_root(relative: &str) -> Option<PathBuf> {
        let root = crate::test_support::formula_dataset_root()?;
        let path = root.join(relative);
        if path.exists() {
            Some(path)
        } else {
            crate::test_support::asset(
                &format!(
                    "formula dataset `{relative}` under {} (set {TEST_ROOT_ENV})",
                    root.display()
                ),
                None,
            )
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("rapid-ocr-rs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        root
    }

    /// 取测试集子目录，缺失时直接跳过当前测试。
    macro_rules! dataset_root {
        ($relative:expr) => {
            match launcher_root($relative) {
                Some(root) => root,
                None => return,
            }
        };
    }

    #[test]
    fn missing_root_returns_structured_error() {
        let root = temp_root("missing-root").join("does-not-exist");
        let result = load_im2latex(&root, FormulaSplit::Test);
        let err = result.expect_err("missing root must fail");
        assert!(
            err.to_string().contains("does-not-exist")
                || matches!(err, RapidOcrError::FileNotFound(_)),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn missing_label_file_returns_error() {
        let root = dataset_root!("im2latex-100k");
        let result = load_im2latex(&root.join("nope"), FormulaSplit::Test);
        assert!(result.is_err());
    }

    #[test]
    fn im2latex_test_split_locates_all_images() {
        let root = dataset_root!("im2latex-100k");
        let fixture = load_im2latex(&root, FormulaSplit::Test).expect("im2latex test should load");
        assert_eq!(fixture.len(), 10_355);
        assert!(fixture.samples.iter().all(|s| s.image_path.is_file()));
    }

    #[test]
    fn im2latex_validate_split_locates_all_images() {
        let root = dataset_root!("im2latex-100k");
        let fixture =
            load_im2latex(&root, FormulaSplit::Validate).expect("im2latex validate should load");
        assert_eq!(fixture.len(), 8_370);
        assert!(fixture.samples.iter().all(|s| s.image_path.is_file()));
    }

    #[test]
    fn im2latex_test_and_validate_have_no_overlap() {
        let root = dataset_root!("im2latex-100k");
        let test = load_im2latex(&root, FormulaSplit::Test).expect("test should load");
        let validate = load_im2latex(&root, FormulaSplit::Validate).expect("validate should load");
        assert_eq!(overlap_count(&test, &validate), 0);
    }

    #[test]
    fn im2latex_empty_ground_truth_are_flagged_not_dropped() {
        let root = dataset_root!("im2latex-100k");
        let fixture = load_im2latex(&root, FormulaSplit::Test).expect("test should load");
        let empty = fixture.empty_ground_truth().collect::<Vec<_>>();
        assert_eq!(
            empty.len(),
            71,
            "im2latex test has 71 empty-label references"
        );
        let scorable = fixture.scorable().count();
        assert_eq!(scorable, 10_355 - 71);
    }

    #[test]
    fn im2latex_out_of_bounds_index_fails() {
        let root = temp_root("im2latex-oob-test");
        std::fs::write(
            root.join("im2latex_test_filter.lst"),
            "no_such.png 999999\n",
        )
        .expect("write filter");
        std::fs::write(root.join("im2latex_formulas.norm.lst"), "\\int\n").expect("labels");
        let result = load_im2latex(&root, FormulaSplit::Test);
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn im2latex_missing_image_fails_with_name() {
        let root = temp_root("im2latex-missing-img");
        std::fs::write(root.join("im2latex_test_filter.lst"), "no_such.png 0\n")
            .expect("write filter");
        std::fs::write(root.join("im2latex_formulas.norm.lst"), "x\n").expect("write labels");
        let result = load_im2latex(&root, FormulaSplit::Test);
        let err = result.expect_err("must fail");
        assert!(err.to_string().contains("no_such.png"), "error: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn latex_ocr_example_val_has_501_and_valid_images() {
        let root = dataset_root!("ocr_rec_latexocr_dataset_example");
        let fixture =
            load_latex_ocr_example(&root, FormulaSplit::Validate).expect("val split should load");
        assert_eq!(fixture.len(), 501);
        assert!(fixture.samples.iter().all(|s| s.image_path.is_file()));
    }

    #[test]
    fn latex_ocr_example_missing_image_fails() {
        let root = temp_root("latexocr-missing");
        std::fs::create_dir_all(root.join("images")).expect("temp dir");
        std::fs::write(root.join("val.txt"), "images/nope.png\t\\int\n").expect("write val");
        let result = load_latex_ocr_example(&root, FormulaSplit::Validate);
        let err = result.expect_err("must fail");
        assert!(err.to_string().contains("nope.png"), "error: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unimer_spe_maps_filenames_to_label_lines() {
        let root = dataset_root!("UniMER-Test");
        let fixture = load_unimer(&root, UniMerSubset::Spe).expect("spe should load");
        assert_eq!(fixture.len(), 6_762);
        let sample = &fixture.samples[0];
        let rendered = sample
            .image_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(sample.source_index, Some(rendered));
        assert!(fixture.samples.iter().all(|s| s.image_path.is_file()));
    }

    #[test]
    fn unimer_cpe_is_zip_mapped() {
        let root = dataset_root!("UniMER-Test");
        let fixture = load_unimer(&root, UniMerSubset::Cpe).expect("cpe should load");
        assert_eq!(fixture.len(), 5_921);
    }

    #[test]
    fn unimer_sce_maps_filenames_to_label_lines() {
        let root = dataset_root!("UniMER-Test");
        let fixture = load_unimer(&root, UniMerSubset::Sce).expect("sce should load");
        assert_eq!(fixture.len(), 4_742);
    }

    #[test]
    fn unimer_hwe_maps_filenames_to_label_lines() {
        let root = dataset_root!("UniMER-Test");
        let fixture = load_unimer(&root, UniMerSubset::Hwe).expect("hwe should load");
        assert_eq!(fixture.len(), 6_332);
    }

    #[test]
    fn unimer_each_subset_first_middle_last_mapping_and_decodable() {
        let root = dataset_root!("UniMER-Test");
        for subset in [
            UniMerSubset::Spe,
            UniMerSubset::Cpe,
            UniMerSubset::Sce,
            UniMerSubset::Hwe,
        ] {
            let fixture = load_unimer(&root, subset).expect("subset should load");
            assert!(!fixture.is_empty(), "{} empty", subset.as_str());
            let count = fixture.len();
            let pick = [0, count / 2, count - 1];
            for &i in &pick {
                let sample = &fixture.samples[i];
                let stem = sample
                    .image_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                assert_eq!(
                    sample.source_index,
                    Some(stem),
                    "{} sample {}: image name must equal label line index",
                    subset.as_str(),
                    i
                );
                assert!(
                    sample.has_ground_truth(),
                    "{} sample {} has empty ground truth",
                    subset.as_str(),
                    i
                );
                assert!(
                    image::load_from_memory(&fs::read(&sample.image_path).unwrap()).is_ok(),
                    "{} sample {} not decodable: {}",
                    subset.as_str(),
                    i,
                    sample.image_path.display()
                );
            }
        }
    }

    #[test]
    fn unimer_out_of_bounds_index_fails() {
        let result = load_unimer(
            &temp_root("unimer-missing").join("does-not-exist"),
            UniMerSubset::Spe,
        );
        assert!(result.is_err());
    }

    #[test]
    fn unicode_and_space_paths_are_readable() {
        // im2latex filenames are ASCII, but verify loader handles a Unicode-named tree
        // and an image name that contains a space (index is the final token).
        let root = temp_root("unicode-space").join("公式 测试");
        std::fs::create_dir_all(&root).expect("temp dir");
        std::fs::write(root.join("im2latex_formulas.norm.lst"), "a+b\n").expect("labels");
        std::fs::write(root.join("im2latex_test_filter.lst"), "图像 1.png 0\n").expect("filter");
        std::fs::write(root.join("图像 1.png"), "not-really-png").expect("image file");
        let fixture = load_im2latex(&root, FormulaSplit::Test).expect("unicode path should load");
        assert_eq!(fixture.len(), 1);
        assert_eq!(fixture.samples[0].image_path, root.join("图像 1.png"));
        let _ = std::fs::remove_dir_all(&root);
    }
}

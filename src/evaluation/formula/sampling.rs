//! 评测抽样与 manifest 稳定性。
//!
//! 公式主评测必须“测试顺序稳定、可重复生成相同 manifest/hash”，并且不能只报告
//! 对模型有利的样本。因此：
//!
//! - 抽样使用**数据集相对路径 + 真值的稳定哈希排序**，而不是“取前 N 张”、
//!   时间戳随机或绝对路径：同一数据集在任何机器、任何绝对路径下都选出同一子集；
//! - 图像**内容**摘要单独记录在 `content_sha256` / `ManifestEntry::image_sha256`，
//!   用于检测“路径与标签不变但文件被替换”。内容不参与样本选择，
//!   否则数据一改动就无法再用既有 manifest 判断“换的是内容还是选的样本”。
//! - manifest 记录每个样本的相对路径与真值 SHA-256，并给出整体哈希，
//!   重跑必须得到同一个哈希。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evaluation::formula::fixture::FormulaSample;

/// 抽样策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleStrategy {
    /// 按 `相对路径 + 真值` 的稳定哈希排序后取前 N 条：与文件系统顺序、绝对路径无关。
    Hash,
    /// 文件/标签的原始顺序取前 N 条（仅用于复现历史 smoke 结果）。
    First,
}

impl SampleStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hash => "hash",
            Self::First => "first",
        }
    }
}

/// 抽样键：数据集相对路径 + 真值。
///
/// **不含绝对路径**，因此数据集被复制到别处不会改变样本选择。
/// 也不含图像内容，理由见模块文档。
fn sample_key(sample: &FormulaSample) -> String {
    format!("{}\u{1f}{}", sample.relative_path(), sample.ground_truth)
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 选择评测样本；`limit == 0` 表示全量。
///
/// 返回值保持“评测执行顺序”，`Hash` 策略下即按哈希升序，因此顺序本身也是稳定的。
pub fn select_samples(
    samples: &[FormulaSample],
    strategy: SampleStrategy,
    limit: usize,
) -> Vec<&FormulaSample> {
    match strategy {
        SampleStrategy::First => samples
            .iter()
            .take(if limit == 0 { usize::MAX } else { limit })
            .collect(),
        SampleStrategy::Hash => {
            let mut keyed: Vec<(String, &FormulaSample)> = samples
                .iter()
                .map(|s| (digest_hex(sample_key(s).as_bytes()), s))
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            let take = if limit == 0 { usize::MAX } else { limit };
            keyed.into_iter().take(take).map(|(_, s)| s).collect()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// 相对数据集根目录的路径，使用 `/` 分隔，保证跨平台哈希一致。
    pub relative_path: String,
    pub ground_truth_sha256: String,
    pub has_ground_truth: bool,
    /// 图像文件内容的 SHA-256；文件不可读时为 `None`（不阻塞 manifest 生成）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub dataset: String,
    pub split: String,
    pub subset: Option<String>,
    pub strategy: SampleStrategy,
    pub limit: usize,
    pub entry_count: usize,
    pub scored_count: usize,
    /// 样本选择摘要：数据集 / 切分 / 子集 / 抽样策略 / 数量 + 每个样本的
    /// `relative_path` 与真值摘要**（按评测顺序）**。
    ///
    /// 它只取决于“选了哪些样本、以什么顺序评测”，与图像文件内容无关。
    /// 注意它**包含顺序**：改变抽样键（例如从绝对路径改为相对路径）会改变该值，
    /// 即使选出的样本集合完全相同。判断“是不是同一批样本”应使用
    /// [`Manifest::sample_set_sha256`]。
    pub manifest_sha256: String,
    /// **顺序无关**的样本集合摘要：把 `relative_path` + 真值摘要排序后再哈希。
    ///
    /// 抽样键或执行顺序变化时该值不变，因此历史报告可以用它对齐到重新生成的
    /// manifest，从而区分“换了一批样本”和“只是换了顺序”。
    ///
    /// `default`：该字段是后来加入的，旧 manifest 里没有；读取旧报告时留空，
    /// 由合并逻辑给出「需要重新生成」的可定位错误，而不是反序列化失败。
    #[serde(default)]
    pub sample_set_sha256: String,
    /// 图像内容摘要：在样本集合摘要之上再纳入每个样本的图像文件 SHA-256
    /// （同样按 `relative_path` 排序，因此与评测顺序无关）。
    ///
    /// 路径与标签不变、但图像文件被替换时，`sample_set_sha256` 不变而本字段变化，
    /// 因此 `--expect-manifest` 能发现数据被改动。旧 manifest（本字段为 `None`）
    /// 仍可用来固定样本集合，只是不校验内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    pub entries: Vec<ManifestEntry>,
}

/// 构建 manifest 并计算稳定哈希。
///
/// `relative_path` 直接取样本在加载时校验过的数据集相对路径（与抽样键同源）：
/// manifest 与实际选择的样本不会分叉，也不需要再次剥离根目录。
///
/// 会为每个样本计算图像文件 SHA-256（`content_sha256` 用），文件不可读时该条目
/// 的图像摘要为 `None`，并且不写入 `content_sha256`（避免给出“已校验”的假象）。
pub fn build_manifest(
    dataset: &str,
    split: &str,
    subset: Option<&str>,
    strategy: SampleStrategy,
    limit: usize,
    samples: &[&FormulaSample],
) -> Manifest {
    let entries: Vec<ManifestEntry> = samples
        .iter()
        .map(|sample| ManifestEntry {
            relative_path: sample.relative_path().to_string(),
            ground_truth_sha256: digest_hex(sample.ground_truth.as_bytes()),
            has_ground_truth: sample.has_ground_truth(),
            image_sha256: crate::model_store::sha256_file(&sample.image_path).ok(),
        })
        .collect();

    let mut canonical = String::new();
    canonical.push_str(dataset);
    canonical.push('\u{1f}');
    canonical.push_str(split);
    canonical.push('\u{1f}');
    canonical.push_str(subset.unwrap_or(""));
    canonical.push('\u{1f}');
    canonical.push_str(strategy.as_str());
    canonical.push('\u{1f}');
    canonical.push_str(&limit.to_string());
    for entry in &entries {
        canonical.push('\u{1e}');
        canonical.push_str(&entry.relative_path);
        canonical.push('\u{1f}');
        canonical.push_str(&entry.ground_truth_sha256);
    }

    // 顺序无关的样本集合摘要：先按 `relative_path` 排序，再哈希。
    let mut sorted_paths: Vec<(&str, &str)> = entries
        .iter()
        .map(|entry| {
            (
                entry.relative_path.as_str(),
                entry.ground_truth_sha256.as_str(),
            )
        })
        .collect();
    sorted_paths.sort_unstable();
    let mut set_canonical = String::new();
    set_canonical.push_str(dataset);
    set_canonical.push('\u{1f}');
    set_canonical.push_str(split);
    set_canonical.push('\u{1f}');
    set_canonical.push_str(subset.unwrap_or(""));
    set_canonical.push('\u{1f}');
    set_canonical.push_str(strategy.as_str());
    set_canonical.push('\u{1f}');
    set_canonical.push_str(&limit.to_string());
    for (path, truth) in &sorted_paths {
        set_canonical.push('\u{1e}');
        set_canonical.push_str(path);
        set_canonical.push('\u{1f}');
        set_canonical.push_str(truth);
    }
    let sample_set_sha256 = digest_hex(set_canonical.as_bytes());

    // 内容摘要复用集合摘要，再按同一排序追加图像摘要，便于审查“只差图像内容”。
    let content_sha256 = entries
        .iter()
        .all(|entry| entry.image_sha256.is_some())
        .then(|| {
            let mut sorted_images: Vec<(&str, &str)> = entries
                .iter()
                .map(|entry| {
                    (
                        entry.relative_path.as_str(),
                        entry.image_sha256.as_deref().unwrap_or(""),
                    )
                })
                .collect();
            sorted_images.sort_unstable();
            let mut content = set_canonical.clone();
            for (path, image) in &sorted_images {
                content.push('\u{1e}');
                content.push_str(path);
                content.push('\u{1f}');
                content.push_str(image);
            }
            digest_hex(content.as_bytes())
        });

    Manifest {
        dataset: dataset.to_string(),
        split: split.to_string(),
        subset: subset.map(str::to_string),
        strategy,
        limit,
        entry_count: entries.len(),
        scored_count: entries
            .iter()
            .filter(|entry| entry.has_ground_truth)
            .count(),
        manifest_sha256: digest_hex(canonical.as_bytes()),
        sample_set_sha256,
        content_sha256,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::evaluation::formula::fixture::{
        FormulaDataset, FormulaSample, FormulaSplit, dataset_relative_path,
    };

    fn samples(count: usize) -> Vec<FormulaSample> {
        (0..count)
            .map(|index| {
                FormulaSample::new(
                    format!("image_{index}.png"),
                    PathBuf::from(format!("root/image_{index}.png")),
                    format!("x_{index}"),
                    FormulaDataset::Im2Latex,
                    FormulaSplit::Test,
                    Some(index),
                )
                .expect("sample")
            })
            .collect()
    }

    /// 数据集必须自包含：清单里指向根目录之外的图像必须**报错**，
    /// 而不是把绝对路径当成“相对路径”泄漏进抽样键与 manifest。
    #[test]
    fn dataset_relative_path_rejects_images_outside_the_root() {
        let root =
            std::env::temp_dir().join(format!("rapid-ocr-rs-rootcheck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let inner = root.join("images");
        std::fs::create_dir_all(&inner).expect("root");
        let inside = inner.join("a.png");
        std::fs::write(&inside, b"x").expect("write");

        // 根目录之内：得到 `/` 分隔的相对路径，且与根的写法无关。
        assert_eq!(
            dataset_relative_path(&root, &inside).expect("inside root"),
            "images/a.png"
        );
        assert_eq!(
            dataset_relative_path(&root.join("."), &inside).expect("normalized root"),
            "images/a.png"
        );

        // 根目录之外：必须失败，并且错误信息能定位到图像与数据集根。
        let outside = std::env::temp_dir().join("rapid-ocr-rs-outside.png");
        std::fs::write(&outside, b"x").expect("write outside");
        let error = dataset_relative_path(&root, &outside)
            .expect_err("images outside the dataset root must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("outside the dataset root"),
            "error: {message}"
        );

        // 调用方必须传入已 join 到根目录的绝对路径；裸相对路径在这里没有明确含义，
        // 因此同样被拒绝（而不是被猜测成“已经是相对路径”）。
        assert!(
            dataset_relative_path(&root, Path::new("images/a.png")).is_err(),
            "a bare relative path has no defined meaning and must be rejected"
        );

        // `FormulaSample::new` 拒绝绝对路径与空路径（双保险）。
        assert!(
            FormulaSample::new(
                "D:/elsewhere/a.png".to_string(),
                inside.clone(),
                "x".to_string(),
                FormulaDataset::Im2Latex,
                FormulaSplit::Test,
                None,
            )
            .is_err(),
            "absolute relative_path must be rejected"
        );
        assert!(
            FormulaSample::new(
                String::new(),
                inside,
                "x".to_string(),
                FormulaDataset::Im2Latex,
                FormulaSplit::Test,
                None,
            )
            .is_err(),
            "empty relative_path must be rejected"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn hash_sampling_is_deterministic_and_independent_of_input_order() {
        let original = samples(50);
        let mut shuffled = original.clone();
        shuffled.reverse();

        let first = select_samples(&original, SampleStrategy::Hash, 10);
        let second = select_samples(&shuffled, SampleStrategy::Hash, 10);
        let first_paths: Vec<_> = first.iter().map(|s| s.image_path.clone()).collect();
        let second_paths: Vec<_> = second.iter().map(|s| s.image_path.clone()).collect();
        assert_eq!(
            first_paths, second_paths,
            "hash sampling must be order independent"
        );
        assert_eq!(first_paths.len(), 10);
    }

    /// 抽样键必须只依赖数据集相对路径与真值：把数据集搬到别的绝对路径下，
    /// 选出的样本与顺序都不能变（跨机器复现的前提）。
    #[test]
    fn hash_sampling_is_independent_of_the_absolute_root() {
        let relocate = |prefix: &str| -> Vec<FormulaSample> {
            (0..40)
                .map(|index| {
                    FormulaSample::new(
                        format!("images/image_{index}.png"),
                        PathBuf::from(format!("{prefix}/images/image_{index}.png")),
                        format!("x_{index}"),
                        FormulaDataset::Im2Latex,
                        FormulaSplit::Test,
                        Some(index),
                    )
                    .expect("sample")
                })
                .collect()
        };
        let original = relocate("D:/datasets/Formula-TestSet");
        let moved = relocate("C:/other/place/Formula-TestSet");

        let first = select_samples(&original, SampleStrategy::Hash, 12);
        let second = select_samples(&moved, SampleStrategy::Hash, 12);
        let first_keys: Vec<&str> = first.iter().map(|s| s.relative_path()).collect();
        let second_keys: Vec<&str> = second.iter().map(|s| s.relative_path()).collect();
        assert_eq!(
            first_keys, second_keys,
            "sample selection must not depend on the absolute dataset root"
        );

        // 全量抽样（limit = 0）下集合相同、顺序也必须相同。
        let all_first: Vec<&str> = select_samples(&original, SampleStrategy::Hash, 0)
            .iter()
            .map(|s| s.relative_path())
            .collect();
        let all_second: Vec<&str> = select_samples(&moved, SampleStrategy::Hash, 0)
            .iter()
            .map(|s| s.relative_path())
            .collect();
        assert_eq!(all_first, all_second);

        // manifest 也必须一致（相对路径同源，内容相同）。
        let selected_a: Vec<&FormulaSample> = select_samples(&original, SampleStrategy::Hash, 0);
        let selected_b: Vec<&FormulaSample> = select_samples(&moved, SampleStrategy::Hash, 0);
        let manifest_a = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::Hash,
            0,
            &selected_a,
        );
        let manifest_b = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::Hash,
            0,
            &selected_b,
        );
        assert_eq!(
            manifest_a.manifest_sha256, manifest_b.manifest_sha256,
            "manifest hash must be root independent"
        );
    }

    #[test]
    fn hash_sampling_is_not_first_n() {
        let all = samples(50);
        let hashed = select_samples(&all, SampleStrategy::Hash, 10);
        let first = select_samples(&all, SampleStrategy::First, 10);
        let hashed_paths: Vec<_> = hashed.iter().map(|s| s.image_path.clone()).collect();
        let first_paths: Vec<_> = first.iter().map(|s| s.image_path.clone()).collect();
        assert_ne!(
            hashed_paths, first_paths,
            "hash sampling must not degenerate into first-N selection"
        );
    }

    #[test]
    fn zero_limit_selects_everything() {
        let all = samples(20);
        assert_eq!(select_samples(&all, SampleStrategy::Hash, 0).len(), 20);
        assert_eq!(select_samples(&all, SampleStrategy::First, 0).len(), 20);
    }

    /// 图像内容变化必须改变 `content_sha256`，但**不**改变样本选择摘要。
    #[test]
    fn content_hash_detects_replaced_image_files() {
        let workspace = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-manifest-content-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).expect("temp dir");
        let image = workspace.join("image.png");
        std::fs::write(&image, b"first-bytes").expect("write image");

        let sample = FormulaSample::new(
            "image.png".to_string(),
            image.clone(),
            "x".to_string(),
            FormulaDataset::Im2Latex,
            FormulaSplit::Test,
            Some(0),
        )
        .expect("sample");
        let selected = [&sample];
        let before = build_manifest("im2latex", "test", None, SampleStrategy::Hash, 0, &selected);
        assert!(
            before.content_sha256.is_some(),
            "readable images must produce a content digest"
        );
        assert_eq!(
            before.entries[0].image_sha256.as_deref(),
            before.content_sha256.as_deref().map(|_| before.entries[0]
                .image_sha256
                .as_deref()
                .expect("image hash"))
        );

        // 路径与标签不变，只替换文件内容。
        std::fs::write(&image, b"second-bytes").expect("rewrite image");
        let after = build_manifest("im2latex", "test", None, SampleStrategy::Hash, 0, &selected);

        assert_eq!(
            before.manifest_sha256, after.manifest_sha256,
            "replacing image bytes must not change the sample-selection digest"
        );
        assert_ne!(
            before.entries[0].image_sha256, after.entries[0].image_sha256,
            "the per-entry image digest must change"
        );
        assert_ne!(
            before.content_sha256, after.content_sha256,
            "the content digest must change so --expect-manifest can detect it"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// 图像不可读时不得伪造内容摘要。
    #[test]
    fn missing_images_do_not_produce_a_content_digest() {
        let sample = FormulaSample::new(
            "does-not-exist.png".to_string(),
            PathBuf::from("root/does-not-exist.png"),
            "x".to_string(),
            FormulaDataset::Im2Latex,
            FormulaSplit::Test,
            None,
        )
        .expect("sample");
        let manifest = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::First,
            0,
            &[&sample],
        );
        assert!(manifest.entries[0].image_sha256.is_none());
        assert!(
            manifest.content_sha256.is_none(),
            "a manifest with unreadable images must not claim content verification"
        );
    }

    #[test]
    fn manifest_hash_is_stable_and_sensitive_to_content() {
        let all = samples(8);
        let selected = select_samples(&all, SampleStrategy::Hash, 0);
        let first = build_manifest("im2latex", "test", None, SampleStrategy::Hash, 0, &selected);
        let second = build_manifest("im2latex", "test", None, SampleStrategy::Hash, 0, &selected);
        assert_eq!(first.manifest_sha256, second.manifest_sha256);
        assert_eq!(first.entry_count, 8);
        let paths: std::collections::BTreeSet<&str> = first
            .entries
            .iter()
            .map(|entry| entry.relative_path.as_str())
            .collect();
        assert_eq!(
            paths.len(),
            8,
            "manifest must list every selected sample once"
        );
        assert!(
            paths.contains("image_0.png") && paths.contains("image_7.png"),
            "manifest must keep every selected sample: {paths:?}"
        );

        let mut mutated: Vec<FormulaSample> = selected.iter().map(|s| (*s).clone()).collect();
        mutated[0].ground_truth = "different".to_string();
        let mutated_refs: Vec<&FormulaSample> = mutated.iter().collect();
        let third = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::Hash,
            0,
            &mutated_refs,
        );
        assert_ne!(
            first.manifest_sha256, third.manifest_sha256,
            "manifest hash must change when ground truth changes"
        );
    }

    #[test]
    fn manifest_uses_forward_slashes_on_windows_paths() {
        let sample = FormulaSample::new(
            "sub/image.png".to_string(),
            PathBuf::from(r"root\sub\image.png"),
            "x".to_string(),
            FormulaDataset::Im2Latex,
            FormulaSplit::Test,
            None,
        )
        .expect("sample");
        let manifest = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::First,
            0,
            &[&sample],
        );
        assert_eq!(manifest.entries[0].relative_path, "sub/image.png");
    }
}

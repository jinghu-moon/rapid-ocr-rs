//! 评测抽样与 manifest 稳定性。
//!
//! 公式主评测必须“测试顺序稳定、可重复生成相同 manifest/hash”，并且不能只报告
//! 对模型有利的样本。因此：
//!
//! - 抽样使用 **内容哈希排序**，而不是“取前 N 张”或时间戳随机；
//! - 同一数据集/切分/子集/数量在任何机器上产生同一子集；
//! - manifest 记录每个样本的相对路径与真值 SHA-256，并给出整体哈希，
//!   重跑必须得到同一个哈希。

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evaluation::formula::fixture::FormulaSample;

/// 抽样策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleStrategy {
    /// 按内容哈希排序后取前 N 条：稳定、与文件系统顺序无关。
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

fn sample_key(sample: &FormulaSample) -> String {
    format!(
        "{}\u{1f}{}",
        sample.image_path.to_string_lossy(),
        sample.ground_truth
    )
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
    pub manifest_sha256: String,
    pub entries: Vec<ManifestEntry>,
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 构建 manifest 并计算稳定哈希。
pub fn build_manifest(
    dataset: &str,
    split: &str,
    subset: Option<&str>,
    strategy: SampleStrategy,
    limit: usize,
    root: &Path,
    samples: &[&FormulaSample],
) -> Manifest {
    let entries: Vec<ManifestEntry> = samples
        .iter()
        .map(|sample| ManifestEntry {
            relative_path: relative_path(root, &sample.image_path),
            ground_truth_sha256: digest_hex(sample.ground_truth.as_bytes()),
            has_ground_truth: sample.has_ground_truth(),
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
        entries,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::evaluation::formula::fixture::{FormulaDataset, FormulaSample, FormulaSplit};

    fn samples(count: usize) -> Vec<FormulaSample> {
        (0..count)
            .map(|index| {
                FormulaSample::new(
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

    #[test]
    fn manifest_hash_is_stable_and_sensitive_to_content() {
        let all = samples(8);
        let selected = select_samples(&all, SampleStrategy::Hash, 0);
        let root = PathBuf::from("root");
        let first = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::Hash,
            0,
            &root,
            &selected,
        );
        let second = build_manifest(
            "im2latex",
            "test",
            None,
            SampleStrategy::Hash,
            0,
            &root,
            &selected,
        );
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
            &root,
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
            Path::new("root"),
            &[&sample],
        );
        assert_eq!(manifest.entries[0].relative_path, "sub/image.png");
    }
}

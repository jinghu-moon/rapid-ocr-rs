//! PP-FormulaNet_plus 主评测工具（任务文档阶段 9）。
//!
//! `--dataset-root` 指向公式测试集**根目录**（例如 `<workspace>/Formula-TestSet`），
//! 工具按 `--dataset` 拼接标准子目录 `im2latex-100k` / `ocr_rec_latexocr_dataset_example` /
//! `UniMER-Test`。
//!
//! ```powershell
//! # PaddleX 示例集 501 张 smoke
//! cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
//!   --dataset latexocr --split validate --output target/formula-val-501.json
//! # im2latex 100 张固定 smoke（内容哈希抽样）
//! cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
//!   --dataset im2latex --split test --limit 100 --output target/formula-im2latex-100.json
//! # im2latex 完整测试集
//! cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
//!   --dataset im2latex --split test --output target/formula-im2latex-full.json
//! # UniMER 分组（逐个 subset 运行，禁止混合汇总）
//! cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
//!   --dataset unimer --subset spe --output target/formula-unimer-spe.json
//! ```
//!
//! 报告包含稳定抽样 manifest 与哈希、exact/normalized match、CER、EOS/truncated、
//! 失败分类、吞吐与 P50/P95、峰值内存、provider 与线程设置，以及可选的
//! Rust/Python 链路对比。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, ValueEnum};
use rapid_ocr_rs::{
    FormulaRecognizer, ProviderPreference, RapidOcrError, RuntimeConfig,
    evaluation::{
        formula::{
            fixture::{
                FormulaFixture, FormulaSample, FormulaSplit, UniMerSubset, load_im2latex,
                load_latex_ocr_example, load_unimer,
            },
            report::{EvaluationSummary, FailureKind, SampleRecord, summarize},
            sampling::{Manifest, SampleStrategy, build_manifest, select_samples},
        },
        stats::Stats,
    },
    peak_memory_source, peak_working_set_bytes, sha256_file,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DatasetArg {
    Im2latex,
    Latexocr,
    Unimer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SubsetArg {
    Spe,
    Cpe,
    Sce,
    Hwe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SampleArg {
    Hash,
    First,
}

#[derive(Debug, Parser)]
#[command(
    name = "formula_eval",
    about = "Evaluate PP-FormulaNet_plus over im2latex / latexocr / UniMER with stable manifests"
)]
struct Cli {
    /// 公式识别模型；合并分片报告时不需要。
    #[arg(long, required_unless_present = "merge_shards")]
    model: Option<PathBuf>,
    /// 公式测试集根目录，例如 `<workspace>/Formula-TestSet`；合并分片报告时不需要。
    #[arg(long = "dataset-root")]
    dataset_root: Option<PathBuf>,
    /// 合并分片报告时不需要（数据集信息从分片报告读取）。
    #[arg(long, value_enum)]
    dataset: Option<DatasetArg>,
    #[arg(long, default_value = "test")]
    split: String,
    /// `--dataset unimer` 必填。
    #[arg(long, value_enum)]
    subset: Option<SubsetArg>,
    /// 0 表示全量。
    #[arg(long, default_value_t = 0)]
    limit: usize,
    #[arg(long = "batch-size", default_value_t = 8)]
    batch_size: usize,
    #[arg(long, value_enum, default_value_t = SampleArg::Hash)]
    sample: SampleArg,
    /// 校验当前选择是否与既有 manifest 完全一致（哈希比较），用于复现固定子集。
    #[arg(long = "expect-manifest")]
    expect_manifest: Option<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long = "manifest-output")]
    manifest_output: Option<PathBuf>,
    /// Python 参考 JSON（`tools/formula_reference.py` 输出），用于链路对比。
    #[arg(long = "python-reference")]
    python_reference: Option<PathBuf>,
    #[arg(long = "expected-sha256")]
    expected_sha256: Option<String>,
    /// 覆盖 ONNX Runtime intra-op 线程数。
    #[arg(long)]
    threads: Option<usize>,
    /// 每处理多少张图片输出一次进度。
    #[arg(long = "progress-every", default_value_t = 500)]
    progress_every: usize,
    /// 不在报告中写入逐样本记录（仅保留汇总）；失败样本将无法逐个审查。
    #[arg(long = "no-records")]
    no_records: bool,
    /// 只评测 `INDEX/COUNT` 这一片（按 manifest 顺序取模分片）。
    ///
    /// manifest 与抽样仍然覆盖**完整**集合，因此所有分片写出同一个
    /// `manifest_sha256`，可用 `--merge-shards` 合并成与串行运行等价的报告。
    /// 分片只影响执行方式，不影响样本集合、指标口径或精度。
    #[arg(long, value_name = "INDEX/COUNT")]
    shard: Option<String>,
    /// 合并若干分片报告：按 manifest 顺序重排记录并用同一套代码重新汇总。
    #[arg(long = "merge-shards", num_args = 1.., value_name = "REPORT")]
    merge_shards: Vec<PathBuf>,
}

/// 分片信息。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ShardInfo {
    index: usize,
    count: usize,
}

/// 解析 `INDEX/COUNT`。
fn parse_shard(raw: &str) -> Result<ShardInfo, String> {
    let (index, count) = raw
        .split_once('/')
        .ok_or_else(|| format!("--shard expects INDEX/COUNT, got `{raw}`"))?;
    let index: usize = index
        .trim()
        .parse()
        .map_err(|_| format!("--shard index is not a number: `{index}`"))?;
    let count: usize = count
        .trim()
        .parse()
        .map_err(|_| format!("--shard count is not a number: `{count}`"))?;
    if count == 0 {
        return Err("--shard count must be greater than zero".into());
    }
    if index >= count {
        return Err(format!(
            "--shard index {index} must be smaller than count {count}"
        ));
    }
    Ok(ShardInfo { index, count })
}

#[derive(Debug, Serialize, Deserialize)]
struct ModelInfo {
    path: String,
    size_bytes: u64,
    sha256: String,
    load_ms: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProviderInfo {
    requested: String,
    resolved: String,
    fallback_used: bool,
    intra_threads: Option<usize>,
    inter_threads: Option<usize>,
    auto_tune_threads: bool,
    physical_cpus: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ThroughputInfo {
    images_per_second: f64,
    total_wall_ms: f64,
    /// 每张图的端到端耗时分布（`elapsed_ms / batch_size`）。
    per_image_ms: Stats,
    /// 每次 batch 调用的墙钟耗时分布。
    per_batch_ms: Stats,
    batch_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryInfo {
    // source 用 String 以便分片报告可以被反序列化后合并。
    peak_working_set_start_bytes: Option<u64>,
    peak_working_set_end_bytes: Option<u64>,
    delta_bytes: Option<u64>,
    source: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EvaluationReport {
    /// 工具标识；用 `String` 而不是 `&'static str`，否则报告无法反序列化（合并分片需要）。
    tool: String,
    model: ModelInfo,
    provider: ProviderInfo,
    dataset: String,
    split: String,
    subset: Option<String>,
    batch_size: usize,
    manifest: Manifest,
    throughput: ThroughputInfo,
    memory: MemoryInfo,
    summary: EvaluationSummary,
    reference_comparison: Option<ReferenceComparison>,
    /// 本报告是分片运行时记录的分片编号；串行运行或合并后为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shard: Option<ShardInfo>,
    /// 合并来源的报告文件（合并报告才有的溯源信息）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merged_from: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    records: Option<Vec<SampleRecord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReferenceComparison {
    reference_path: String,
    compared: usize,
    missing_in_reference: usize,
    missing_in_rust: usize,
    /// 完整 token 行逐项一致（不做 EOS 截断）。
    full_token_sequence_matches: usize,
    /// EOS 前 token 序列逐项一致；这是任务文档定义的“token 序列一致”。
    eos_prefix_token_matches: usize,
    eos_index_matches: usize,
    truncated_matches: usize,
    latex_matches: usize,
    /// Rust 与 Python 链路一致但都不等于真值的样本数（模型识别错误，不是链路差异）。
    both_wrong: usize,
    /// Rust 与 Python 链路不一致的样本（链路差异），必须逐个给出。
    link_differences: Vec<LinkDifference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LinkDifference {
    relative_path: String,
    rust_tokens: Vec<i64>,
    python_tokens: Vec<i64>,
    rust_latex: Option<String>,
    python_latex: String,
    rust_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PythonRecord {
    relative_path: String,
    latex: String,
    #[serde(default)]
    token_ids: Vec<i64>,
    #[serde(default)]
    eos_index: Option<usize>,
    #[serde(default)]
    truncated: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PythonReference {
    records: Vec<PythonRecord>,
}

/// EOS 前的 token 序列；无 EOS 时返回全部 token。
fn eos_prefix(token_ids: &[i64], eos_index: Option<usize>) -> &[i64] {
    match eos_index {
        Some(index) if index < token_ids.len() => &token_ids[..=index],
        _ => token_ids,
    }
}

const DATASET_SUBDIRS: &[(&str, &str)] = &[
    ("im2latex-100k", "im2latex"),
    ("ocr_rec_latexocr_dataset_example", "latexocr"),
    ("UniMER-Test", "unimer"),
];

fn dataset_subdir(dataset: DatasetArg) -> &'static str {
    match dataset {
        DatasetArg::Im2latex => "im2latex-100k",
        DatasetArg::Latexocr => "ocr_rec_latexocr_dataset_example",
        DatasetArg::Unimer => "UniMER-Test",
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn parse_split(raw: &str) -> Result<FormulaSplit, String> {
    match raw.to_ascii_lowercase().as_str() {
        "train" => Ok(FormulaSplit::Train),
        "test" => Ok(FormulaSplit::Test),
        "val" | "validate" => Ok(FormulaSplit::Validate),
        other => Err(format!("unsupported split `{other}`")),
    }
}

fn unimer_subset(subset: SubsetArg) -> UniMerSubset {
    match subset {
        SubsetArg::Spe => UniMerSubset::Spe,
        SubsetArg::Cpe => UniMerSubset::Cpe,
        SubsetArg::Sce => UniMerSubset::Sce,
        SubsetArg::Hwe => UniMerSubset::Hwe,
    }
}

fn subset_name(subset: Option<SubsetArg>) -> Option<String> {
    subset.map(|value| format!("{value:?}").to_ascii_lowercase())
}

fn dataset_name(dataset: DatasetArg, subset: Option<SubsetArg>) -> String {
    match dataset {
        DatasetArg::Im2latex => "im2latex".to_string(),
        DatasetArg::Latexocr => "latexocr_example".to_string(),
        DatasetArg::Unimer => format!("unimer_{}", subset_name(subset).unwrap_or_default()),
    }
}

fn load_fixture(
    dataset_root: &Path,
    dataset: DatasetArg,
    split: FormulaSplit,
    subset: Option<SubsetArg>,
) -> Result<FormulaFixture, Box<dyn std::error::Error>> {
    let root = dataset_root.join(dataset_subdir(dataset));
    let fixture = match dataset {
        DatasetArg::Im2latex => load_im2latex(&root, split)?,
        DatasetArg::Latexocr => load_latex_ocr_example(&root, split)?,
        DatasetArg::Unimer => {
            let subset = subset.ok_or("--dataset unimer requires --subset spe|cpe|sce|hwe")?;
            // `load_unimer` 的 root 是 UniMER-Test 目录本身。
            load_unimer(&dataset_root.join("UniMER-Test"), unimer_subset(subset))?
        }
    };
    Ok(fixture)
}

fn runtime_config(cli: &Cli) -> RuntimeConfig {
    RuntimeConfig {
        provider_preference: ProviderPreference::Cpu,
        // 评测必须显式失败而不是静默回退，否则指标对应的 provider 不明确。
        fail_if_provider_unavailable: true,
        intra_threads: cli.threads,
        auto_tune_threads: cli.threads.is_none(),
        ..RuntimeConfig::default()
    }
}

fn error_record(path: &Path, expected: &str, error: &RapidOcrError) -> SampleRecord {
    SampleRecord::pipeline_error(
        path.to_string_lossy().to_string(),
        expected.to_string(),
        error,
    )
}

/// 合并分片报告：按 manifest 顺序重排记录，并用同一套汇总实现重新计算指标。
///
/// 这是分片运行保持“单一实现”的关键：分片只改变执行方式，汇总口径仍然只有
/// `evaluation::formula::report::summarize` 一份。合并会校验：
///
/// - 所有分片的 `manifest_sha256` 一致；
/// - 每个分片都带有 `records`（`--no-records` 的报告无法合并）；
/// - 分片集合恰好覆盖 manifest 的每个条目且无重复、无缺失。
fn merge_shard_reports(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut reports = Vec::with_capacity(cli.merge_shards.len());
    for path in &cli.merge_shards {
        let raw = std::fs::read_to_string(path)?;
        let report: EvaluationReport = serde_json::from_str(&raw)?;
        reports.push((path.clone(), report));
    }
    let (first_path, first) = reports
        .first()
        .ok_or("--merge-shards requires at least one report")?;
    if first.records.is_none() {
        return Err(format!(
            "{} has no per-sample records; shards must be produced without --no-records",
            first_path.display()
        )
        .into());
    }
    let manifest_hash = first.manifest.manifest_sha256.clone();
    for (path, report) in &reports {
        if report.manifest.manifest_sha256 != manifest_hash {
            return Err(format!(
                "{} belongs to a different manifest ({} vs {})",
                path.display(),
                report.manifest.manifest_sha256,
                manifest_hash
            )
            .into());
        }
        if report.records.is_none() {
            return Err(format!("{} has no per-sample records", path.display()).into());
        }
        if report.dataset != first.dataset || report.split != first.split {
            return Err(format!(
                "{} covers {}/{} but the first report covers {}/{}",
                path.display(),
                report.dataset,
                report.split,
                first.dataset,
                first.split
            )
            .into());
        }
    }

    // 按 manifest 顺序落位，同时检测重复与缺失。
    let mut positioned: Vec<Option<(SampleRecord, usize)>> =
        (0..first.manifest.entries.len()).map(|_| None).collect();
    let mut position_by_path: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (position, entry) in first.manifest.entries.iter().enumerate() {
        position_by_path.insert(entry.relative_path.clone(), position);
    }
    let mut merged_elapsed: Vec<f64> = Vec::new();
    for (path, report) in &reports {
        for record in report.records.clone().unwrap_or_default() {
            let Some(position) = position_by_path.get(&record.relative_path).copied() else {
                return Err(format!(
                    "{} contains `{}` which is not in the manifest",
                    path.display(),
                    record.relative_path
                )
                .into());
            };
            if positioned[position].is_some() {
                return Err(
                    format!("`{}` appears in more than one shard", record.relative_path).into(),
                );
            }
            if !record.failure.is_pipeline_error() {
                merged_elapsed.push(f64::from(record.elapsed_ms) / record.batch_size.max(1) as f64);
            }
            positioned[position] = Some((record, position));
        }
    }
    let missing: Vec<&str> = positioned
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_none())
        .map(|(position, _)| first.manifest.entries[position].relative_path.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "merged shards are missing {} manifest entries (first: {})",
            missing.len(),
            missing[0]
        )
        .into());
    }

    let records: Vec<SampleRecord> = positioned
        .into_iter()
        .map(|slot| slot.map(|(record, _)| record).expect("checked above"))
        .collect();
    let summary = summarize(records.iter());
    let wall_ms = reports
        .iter()
        .map(|(_, report)| report.throughput.total_wall_ms)
        .fold(0.0_f64, f64::max);

    let merged = EvaluationReport {
        tool: "formula_eval".to_string(),
        model: ModelInfo {
            path: first.model.path.clone(),
            size_bytes: first.model.size_bytes,
            sha256: first.model.sha256.clone(),
            load_ms: first.model.load_ms,
        },
        provider: ProviderInfo {
            requested: first.provider.requested.clone(),
            resolved: first.provider.resolved.clone(),
            fallback_used: first.provider.fallback_used,
            intra_threads: first.provider.intra_threads,
            inter_threads: first.provider.inter_threads,
            auto_tune_threads: first.provider.auto_tune_threads,
            physical_cpus: first.provider.physical_cpus,
        },
        dataset: first.dataset.clone(),
        split: first.split.clone(),
        subset: first.subset.clone(),
        batch_size: first.batch_size,
        manifest: first.manifest.clone(),
        throughput: ThroughputInfo {
            images_per_second: if wall_ms > 0.0 {
                records.len() as f64 / (wall_ms / 1000.0)
            } else {
                0.0
            },
            total_wall_ms: wall_ms,
            per_image_ms: Stats::from_samples(merged_elapsed),
            per_batch_ms: Stats::from_samples(Vec::new()),
            batch_size: first.batch_size,
        },
        memory: MemoryInfo {
            peak_working_set_start_bytes: reports
                .iter()
                .filter_map(|(_, report)| report.memory.peak_working_set_start_bytes)
                .min(),
            peak_working_set_end_bytes: reports
                .iter()
                .filter_map(|(_, report)| report.memory.peak_working_set_end_bytes)
                .max(),
            delta_bytes: None,
            source: peak_memory_source().to_string(),
        },
        summary,
        reference_comparison: first.reference_comparison.clone(),
        shard: None,
        merged_from: Some(
            reports
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect(),
        ),
        records: Some(records),
    };

    println!(
        "merged: shards={} total={} scored={} exact={:.4} normalized={:.4} mean_cer={:.4} \
         pipeline_failures={} model_mismatches={} truncated={} wall_ms={:.1}",
        reports.len(),
        merged.summary.total,
        merged.summary.scored,
        merged.summary.exact_match_rate,
        merged.summary.normalized_match_rate,
        merged.summary.mean_cer,
        merged.summary.pipeline_failures,
        merged.summary.model_mismatches,
        merged.summary.truncated,
        merged.throughput.total_wall_ms
    );
    match &cli.output {
        Some(path) => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_string_pretty(&merged)?)?;
        }
        None => println!("{}", serde_json::to_string(&merged)?),
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if !cli.merge_shards.is_empty() {
        return merge_shard_reports(&cli);
    }
    if cli.batch_size == 0 {
        return Err("--batch-size must be greater than zero".into());
    }
    let split = parse_split(&cli.split)?;
    let model = cli.model.clone().ok_or("--model is required")?;
    let dataset_root = cli
        .dataset_root
        .clone()
        .ok_or("--dataset-root is required")?;
    let dataset = cli.dataset.ok_or("--dataset is required")?;
    if !dataset_root.is_dir() {
        return Err(format!(
            "--dataset-root {} is not a directory ({} is also checked)",
            dataset_root.display(),
            DATASET_SUBDIRS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }
    let memory_start = peak_working_set_bytes();

    let fixture = load_fixture(&dataset_root, dataset, split, cli.subset)?;
    let strategy = match cli.sample {
        SampleArg::Hash => SampleStrategy::Hash,
        SampleArg::First => SampleStrategy::First,
    };
    let selected = select_samples(&fixture.samples, strategy, cli.limit);
    if selected.is_empty() {
        return Err("no samples selected".into());
    }
    let subset = subset_name(cli.subset);
    let manifest = build_manifest(
        &dataset_name(dataset, cli.subset),
        fixture.split.as_str(),
        subset.as_deref(),
        strategy,
        cli.limit,
        &fixture.root,
        &selected,
    );
    if let Some(path) = &cli.expect_manifest {
        let raw = std::fs::read_to_string(path)?;
        let expected_manifest: Manifest = serde_json::from_str(&raw)?;
        if expected_manifest.manifest_sha256 != manifest.manifest_sha256 {
            return Err(format!(
                "manifest mismatch: {} has {}, current selection hashes to {}",
                path.display(),
                expected_manifest.manifest_sha256,
                manifest.manifest_sha256
            )
            .into());
        }
    }
    if let Some(path) = &cli.manifest_output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    }

    let shard = match cli.shard.as_deref() {
        Some(raw) => Some(parse_shard(raw)?),
        None => None,
    };
    let runtime = runtime_config(&cli);
    let loaded_start = Instant::now();
    let mut recognizer =
        FormulaRecognizer::from_model_with_hash(&model, &runtime, cli.expected_sha256.as_deref())?;
    let load_ms = loaded_start.elapsed().as_secs_f64() * 1000.0;
    let resolution = recognizer.provider_resolution();

    // 分片只裁剪“要评测哪些下标”，manifest 始终覆盖完整集合，
    // 因此所有分片的 manifest_sha256 相同，合并结果与串行运行等价。
    let order: Vec<usize> = match shard {
        Some(info) => (0..selected.len())
            .filter(|index| index % info.count == info.index)
            .collect(),
        None => (0..selected.len()).collect(),
    };
    if order.is_empty() {
        return Err("this shard has no samples".into());
    }

    println!(
        "formula_eval: dataset={} split={} samples={} scored={} batch={} manifest={}{}",
        manifest.dataset,
        fixture.split.as_str(),
        manifest.entry_count,
        manifest.scored_count,
        cli.batch_size,
        &manifest.manifest_sha256[..16],
        match shard {
            Some(info) => format!(
                " shard={}/{} evaluating={}",
                info.index,
                info.count,
                order.len()
            ),
            None => String::new(),
        }
    );

    let total_start = Instant::now();
    let mut records: Vec<SampleRecord> = Vec::with_capacity(order.len());
    let mut per_batch_ms: Vec<f64> = Vec::new();
    let mut per_image_ms: Vec<f64> = Vec::new();
    let batch_size = cli.batch_size;
    let progress_every = cli.progress_every.max(1);

    let mut index = 0usize;
    while index < order.len() {
        let end = (index + batch_size).min(order.len());
        let chunk: Vec<&FormulaSample> = order[index..end]
            .iter()
            .map(|position| selected[*position])
            .collect();
        let mut pending: Vec<Option<SampleRecord>> = (0..chunk.len()).map(|_| None).collect();
        let mut valid_images = Vec::new();
        let mut valid_indices = Vec::new();

        for (local, sample) in chunk.iter().enumerate() {
            match image::open(&sample.image_path) {
                Ok(image) => {
                    valid_images.push(image);
                    valid_indices.push(local);
                }
                Err(error) => {
                    let ocr_error = RapidOcrError::InvalidImage(error.to_string());
                    pending[local] = Some(error_record(
                        &sample.image_path,
                        &sample.ground_truth,
                        &ocr_error,
                    ));
                }
            }
        }

        if !valid_images.is_empty() {
            let started = Instant::now();
            match recognizer.recognize_batch(&valid_images) {
                Ok(results) => {
                    for (result, local) in results.iter().zip(valid_indices.iter().copied()) {
                        let sample = &chunk[local];
                        let record = SampleRecord {
                            image: sample.image_path.to_string_lossy().to_string(),
                            relative_path: String::new(),
                            expected: sample.ground_truth.clone(),
                            actual: Some(result.latex.clone()),
                            token_ids: result.token_ids.clone(),
                            eos_index: result.eos_index,
                            truncated: Some(result.truncated),
                            elapsed_ms: result.elapsed_ms,
                            batch_size: result.batch_size,
                            failure: FailureKind::None,
                            error: None,
                            exact_match: false,
                            normalized_match: false,
                            edit_distance: 0,
                            cer: 1.0,
                            python_token_match: None,
                            python_latex_match: None,
                        };
                        pending[local] = Some(score_record(record, sample.ground_truth.as_str()));
                        per_image_ms
                            .push(f64::from(result.elapsed_ms) / result.batch_size.max(1) as f64);
                    }
                }
                Err(error) => {
                    for local in valid_indices.iter().copied() {
                        let sample = &chunk[local];
                        pending[local] = Some(error_record(
                            &sample.image_path,
                            &sample.ground_truth,
                            &error,
                        ));
                    }
                }
            }
            per_batch_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }

        records.extend(pending.into_iter().flatten());
        index = end;
        if index.is_multiple_of(progress_every) || index == order.len() {
            eprintln!(
                "  progress {index}/{} ({:.1}%)",
                order.len(),
                index as f64 * 100.0 / order.len() as f64
            );
        }
    }

    let total_wall_ms = total_start.elapsed().as_secs_f64() * 1000.0;
    let memory_end = peak_working_set_bytes();

    // 逐样本记录必须能对应到 manifest 条目：串行时顺序一致，分片时是 manifest
    // 顺序的子序列，因此都按相对路径反查下标，保证合并后顺序稳定。
    for record in records.iter_mut() {
        let key = record.image.replace('\\', "/");
        let Some(position) = manifest
            .entries
            .iter()
            .position(|entry| key.ends_with(entry.relative_path.as_str()))
        else {
            return Err(format!(
                "record `{}` does not match any manifest entry",
                record.image
            )
            .into());
        };
        record.relative_path = manifest.entries[position].relative_path.clone();
    }
    if records.len() != order.len() {
        return Err(format!(
            "internal error: {} records for {} evaluated samples",
            records.len(),
            order.len()
        )
        .into());
    }

    let reference_comparison = match &cli.python_reference {
        Some(path) => Some(compare_with_reference(path, &mut records)?),
        None => None,
    };

    let summary = summarize(records.iter());
    let report = EvaluationReport {
        tool: "formula_eval".to_string(),
        model: ModelInfo {
            path: model.display().to_string(),
            size_bytes: std::fs::metadata(&model)?.len(),
            sha256: sha256_file(&model)?,
            load_ms,
        },
        provider: ProviderInfo {
            requested: format!("{:?}", resolution.requested),
            resolved: format!("{:?}", resolution.resolved),
            fallback_used: resolution.fallback_used,
            intra_threads: runtime.intra_threads,
            inter_threads: runtime.inter_threads,
            auto_tune_threads: runtime.auto_tune_threads,
            physical_cpus: num_cpus::get_physical(),
        },
        dataset: manifest.dataset.clone(),
        split: fixture.split.as_str().to_string(),
        subset,
        batch_size,
        manifest,
        throughput: ThroughputInfo {
            images_per_second: if total_wall_ms > 0.0 {
                records.len() as f64 / (total_wall_ms / 1000.0)
            } else {
                0.0
            },
            total_wall_ms,
            per_image_ms: Stats::from_samples(per_image_ms),
            per_batch_ms: Stats::from_samples(per_batch_ms),
            batch_size,
        },
        memory: MemoryInfo {
            peak_working_set_start_bytes: memory_start,
            peak_working_set_end_bytes: memory_end,
            delta_bytes: match (memory_start, memory_end) {
                (Some(start), Some(end)) => Some(end.saturating_sub(start)),
                _ => None,
            },
            source: peak_memory_source().to_string(),
        },
        summary,
        reference_comparison,
        shard,
        merged_from: None,
        records: (!cli.no_records).then_some(records),
    };

    println!(
        "done: total={} scored={} exact={:.4} normalized={:.4} mean_cer={:.4} \
         pipeline_failures={} model_mismatches={} truncated={} load_ms={:.1} wall_ms={:.1}",
        report.summary.total,
        report.summary.scored,
        report.summary.exact_match_rate,
        report.summary.normalized_match_rate,
        report.summary.mean_cer,
        report.summary.pipeline_failures,
        report.summary.model_mismatches,
        report.summary.truncated,
        load_ms,
        report.throughput.total_wall_ms
    );

    let text = serde_json::to_string(&report)?;
    match &cli.output {
        Some(path) => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
        }
        None => println!("{text}"),
    }
    Ok(())
}

/// 计算文本指标与失败分类，写回记录。
fn score_record(mut record: SampleRecord, expected: &str) -> SampleRecord {
    let actual = record.actual.clone().unwrap_or_default();
    let truncated = record.truncated.unwrap_or(false);
    let (metrics, failure) =
        rapid_ocr_rs::evaluation::formula::report::score_latex(expected, &actual, truncated);
    record.failure = failure;
    record.exact_match = metrics.exact_match;
    record.normalized_match = metrics.normalized_match;
    record.edit_distance = metrics.edit_distance;
    record.cer = metrics.cer;
    record
}

fn compare_with_reference(
    path: &Path,
    records: &mut [SampleRecord],
) -> Result<ReferenceComparison, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;
    let reference: PythonReference = serde_json::from_str(&raw)?;
    let by_image: HashMap<String, PythonRecord> = reference
        .records
        .into_iter()
        .map(|record| (record.relative_path.clone(), record))
        .collect();

    let mut comparison = ReferenceComparison {
        reference_path: path.display().to_string(),
        compared: 0,
        missing_in_reference: 0,
        missing_in_rust: 0,
        full_token_sequence_matches: 0,
        eos_prefix_token_matches: 0,
        eos_index_matches: 0,
        truncated_matches: 0,
        latex_matches: 0,
        both_wrong: 0,
        link_differences: Vec::new(),
    };

    let mut matched_paths = HashSet::new();
    for record in records.iter_mut() {
        let Some(reference) = by_image.get(&record.relative_path) else {
            comparison.missing_in_reference += 1;
            continue;
        };
        matched_paths.insert(record.relative_path.clone());
        comparison.compared += 1;

        let full_match = record.token_ids == reference.token_ids;
        let prefix_match = eos_prefix(&record.token_ids, record.eos_index)
            == eos_prefix(&reference.token_ids, reference.eos_index);
        let latex_match = record.actual.as_deref() == Some(reference.latex.as_str());
        record.python_token_match = Some(prefix_match);
        record.python_latex_match = Some(latex_match);
        if full_match {
            comparison.full_token_sequence_matches += 1;
        }
        if prefix_match {
            comparison.eos_prefix_token_matches += 1;
        }
        if record.eos_index == reference.eos_index {
            comparison.eos_index_matches += 1;
        }
        if record.truncated == reference.truncated {
            comparison.truncated_matches += 1;
        }
        if latex_match {
            comparison.latex_matches += 1;
        }
        if latex_match && record.actual.as_deref() != Some(record.expected.as_str()) {
            comparison.both_wrong += 1;
        }
        if !prefix_match || !latex_match {
            comparison.link_differences.push(LinkDifference {
                relative_path: record.relative_path.clone(),
                rust_tokens: record.token_ids.clone(),
                python_tokens: reference.token_ids.clone(),
                rust_latex: record.actual.clone(),
                python_latex: reference.latex.clone(),
                rust_error: record.error.clone(),
            });
        }
    }
    comparison.missing_in_rust = by_image
        .keys()
        .filter(|key| !matched_paths.contains(*key))
        .count();
    Ok(comparison)
}

#[cfg(test)]
mod tests {
    use rapid_ocr_rs::evaluation::formula::fixture::{FormulaSplit, UniMerSubset};

    use super::{
        DatasetArg, SubsetArg, dataset_name, dataset_subdir, load_fixture, parse_shard,
        parse_split, subset_name, unimer_subset,
    };

    #[test]
    fn split_parsing_accepts_val_and_validate() {
        assert_eq!(parse_split("val").expect("val"), FormulaSplit::Validate);
        assert_eq!(
            parse_split("validate").expect("validate"),
            FormulaSplit::Validate
        );
        assert_eq!(parse_split("TEST").expect("test"), FormulaSplit::Test);
        assert!(parse_split("nope").is_err());
    }

    #[test]
    fn unimer_subset_mapping_is_explicit() {
        assert_eq!(unimer_subset(SubsetArg::Spe), UniMerSubset::Spe);
        assert_eq!(unimer_subset(SubsetArg::Hwe), UniMerSubset::Hwe);
        assert_eq!(subset_name(Some(SubsetArg::Sce)).as_deref(), Some("sce"));
    }

    #[test]
    fn dataset_names_are_grouped_by_subset() {
        assert_eq!(dataset_name(DatasetArg::Im2latex, None), "im2latex");
        assert_eq!(
            dataset_name(DatasetArg::Unimer, Some(SubsetArg::Sce)),
            "unimer_sce"
        );
    }

    #[test]
    fn dataset_subdirectories_match_the_documented_layout() {
        assert_eq!(dataset_subdir(DatasetArg::Im2latex), "im2latex-100k");
        assert_eq!(
            dataset_subdir(DatasetArg::Latexocr),
            "ocr_rec_latexocr_dataset_example"
        );
        assert_eq!(dataset_subdir(DatasetArg::Unimer), "UniMER-Test");
    }

    #[test]
    fn fixture_loading_uses_the_collection_root() {
        // 不存在的根目录必须返回可定位错误，而不是 panic。
        let root = std::env::temp_dir().join("rapid-ocr-rs-does-not-exist");
        assert!(load_fixture(&root, DatasetArg::Im2latex, FormulaSplit::Test, None).is_err());
    }

    /// 缺少必填参数时必须给出可定位错误，而不是 panic。
    #[test]
    fn load_fixture_rejects_missing_unimer_subset() {
        let root = std::env::temp_dir();
        assert!(load_fixture(&root, DatasetArg::Unimer, FormulaSplit::Test, None).is_err());
    }

    #[test]
    fn shard_parsing_validates_index_and_count() {
        let shard = parse_shard("1/4").expect("1/4 is valid");
        assert_eq!((shard.index, shard.count), (1, 4));
        assert!(parse_shard("4/4").is_err(), "index must be < count");
        assert!(parse_shard("0/0").is_err(), "count must be > 0");
        assert!(parse_shard("1").is_err(), "INDEX/COUNT is required");
        assert!(parse_shard("a/2").is_err(), "index must be numeric");
    }

    /// 分片划分必须是 manifest 顺序上互不重叠、并集完整的划分。
    #[test]
    fn shard_partition_is_disjoint_and_complete() {
        let total = 37usize;
        let count = 4usize;
        let mut seen = vec![0usize; total];
        for index in 0..count {
            let shard = parse_shard(&format!("{index}/{count}")).expect("valid shard");
            for position in (0..total).filter(|p| p % shard.count == shard.index) {
                seen[position] += 1;
            }
        }
        assert!(
            seen.iter().all(|hits| *hits == 1),
            "every manifest position must belong to exactly one shard: {seen:?}"
        );
    }
}

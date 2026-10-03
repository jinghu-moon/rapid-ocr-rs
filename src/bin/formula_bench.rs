//! PP-FormulaNet_plus 性能与 provider 验收工具。
//!
//! 设计要点（对应任务文档阶段 10）：
//!
//! - 分阶段测量 session 创建、首次推理、warm 预处理 / `session.run` / tokenizer
//!   decode / 端到端，避免初始化或预处理掩盖模型真实性能；
//! - 每个指标记录 **多轮采样** 的 min / max / mean / P50 / P95 / 标准差，而不是只报平均值；
//! - batch=1/2/4/8 分别统计，并给出每张图的分摊耗时；
//! - 记录请求 / 实际 provider 与是否发生 CPU 回退；
//! - 记录进程 **峰值工作集**（Windows PSAPI `GetProcessMemoryInfo.PeakWorkingSetSize`，
//!   全项目唯一口径）；
//! - 公式模型与普通 OCR 的性能**分开报告**，不混成一个吞吐指标；需要对比时通过
//!   `--ocr-baseline` 指向普通 OCR 的 benchmark JSON，只做并列展示，不做加权合并。

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use rapid_ocr_rs::{
    FormulaPreprocessor, FormulaSession, FormulaTokenizer, FormulaTokenizerMetadata,
    ProviderPreference, RuntimeConfig, sha256_file,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Cpu,
    Directml,
    Cuda,
}

#[derive(Debug, Parser)]
#[command(
    name = "formula_bench",
    about = "Benchmark PP-FormulaNet_plus with per-stage latency statistics, providers and peak memory"
)]
struct Cli {
    #[arg(long)]
    model: PathBuf,
    /// 一张或多张真实公式图片；多张图片时 batch 统计会按顺序循环取样。
    #[arg(long = "image", required = true, num_args = 1..)]
    images: Vec<PathBuf>,
    /// 计入统计的测量轮数。
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    /// 不计入统计的预热轮数。
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    /// batch 大小列表。
    #[arg(long = "batch-sizes", value_delimiter = ',', default_value = "1,2,4,8")]
    batch_sizes: Vec<usize>,
    #[arg(long, value_enum, default_value_t = ProviderArg::Cpu)]
    provider: ProviderArg,
    /// 覆盖 ONNX Runtime intra-op 线程数；默认沿用 `RuntimeConfig` 的自动调优。
    #[arg(long)]
    threads: Option<usize>,
    /// 需要校验模型 SHA-256 时传入。
    #[arg(long)]
    expected_sha256: Option<String>,
    /// 普通 OCR benchmark JSON；仅用于并列展示，不参与任何合并计算。
    #[arg(long = "ocr-baseline")]
    ocr_baseline: Option<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
}

use rapid_ocr_rs::evaluation::stats::Stats;
use rapid_ocr_rs::{peak_memory_source, peak_working_set_bytes};

/// 分阶段耗时统计；统计口径来自共享层，避免 benchmark 与评测工具各写一套。
#[derive(Debug, Serialize)]
struct StageStats {
    preprocess: Stats,
    session_run: Stats,
    tokenizer_decode: Stats,
    end_to_end: Stats,
}

impl StageStats {
    fn from_rounds(rounds: &[RoundSample]) -> Self {
        Self {
            preprocess: Stats::from_samples(rounds.iter().map(|r| r.preprocess_ms).collect()),
            session_run: Stats::from_samples(rounds.iter().map(|r| r.run_ms).collect()),
            tokenizer_decode: Stats::from_samples(rounds.iter().map(|r| r.decode_ms).collect()),
            end_to_end: Stats::from_samples(rounds.iter().map(|r| r.e2e_ms).collect()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RoundSample {
    preprocess_ms: f64,
    run_ms: f64,
    decode_ms: f64,
    e2e_ms: f64,
}

#[derive(Debug, Serialize)]
struct BatchReport {
    batch: usize,
    /// 每张图的分摊端到端耗时（e2e / batch）。
    per_image_e2e_ms: Stats,
    /// 整个 batch 的端到端耗时。
    batch_e2e_ms: Stats,
    stages: StageStats,
    /// 相同 batch 的 token/LaTeX 是否稳定（batch 不应改变结果）。
    deterministic_tokens: bool,
    latex: String,
}

#[derive(Debug, Serialize)]
struct MemoryReport {
    peak_working_set_start_bytes: Option<u64>,
    peak_working_set_after_session_bytes: Option<u64>,
    peak_working_set_end_bytes: Option<u64>,
    peak_working_set_delta_bytes: Option<u64>,
    source: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelReport {
    path: String,
    file_name: String,
    size_bytes: u64,
    sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct ThreadReport {
    intra_threads: Option<usize>,
    inter_threads: Option<usize>,
    auto_tune_threads: bool,
    logical_cpus: Option<usize>,
    physical_cpus: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    tool: &'static str,
    provider_requested: String,
    /// 交给 ORT 的 EP 链头部（`selected_ep`）；**不是**逐节点执行证据。
    provider_selected_ep: String,
    provider_fallback_used: bool,
    rounds: usize,
    warmup: usize,
    model: ModelReport,
    images: Vec<String>,
    image_size: Option<[u32; 2]>,
    threads: ThreadReport,
    session_create_ms: f64,
    first_inference_ms: f64,
    warm_single: StageStats,
    batches: Vec<BatchReport>,
    memory: MemoryReport,
    /// 与普通 OCR benchmark 的并列信息；不做任何吞吐合并。
    ocr_baseline: Option<serde_json::Value>,
    notes: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn runtime_config(cli: &Cli) -> RuntimeConfig {
    RuntimeConfig {
        provider_preference: match cli.provider {
            ProviderArg::Cpu => ProviderPreference::Cpu,
            ProviderArg::Directml => ProviderPreference::DirectMl { device_id: 0 },
            ProviderArg::Cuda => ProviderPreference::Cuda { device_id: 0 },
        },
        // benchmark 必须显式失败而不是静默回退，否则数据没有意义。
        fail_if_provider_unavailable: true,
        intra_threads: cli.threads,
        auto_tune_threads: cli.threads.is_none(),
        ..RuntimeConfig::default()
    }
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// 一次测量的结果：耗时样本 + 本次输出张量解码出的 token 序列。
#[derive(Debug, Clone)]
struct Measurement {
    sample: RoundSample,
    /// 每个 batch 行在 **EOS 之前**（含 EOS）的 token 序列。
    ///
    /// 模型图内 `Loop` 会把整个 batch 补齐到同一宽度，因此完整 token 行会随 batch
    /// 组成变化；判定“batch 是否改变结果”必须比较 EOS 前的内容。
    tokens: Vec<Vec<i64>>,
}

/// 把模型输出行截断到 **EOS 之前（含 EOS）**。
///
/// 模型图内 `Loop` 会把整个 batch 补齐到同一宽度，因此完整 token 行的尾部 padding
/// 会随 batch 组成变化。判定“batch 是否改变结果”时，基线与测量必须使用同一种
/// 截断口径，否则会把 padding 差异误报成结果差异。
fn eos_prefix(token_ids: &[i64], eos_index: Option<usize>) -> Vec<i64> {
    match eos_index {
        Some(index) if index < token_ids.len() => token_ids[..=index].to_vec(),
        _ => token_ids.to_vec(),
    }
}

/// 判定一次 batch 的每行是否与同图单图基线一致。
///
/// `rows[i]` 对应 `baselines[i % baselines.len()]`（图片按顺序循环取样）。
fn batch_matches_baselines(baselines: &[Vec<i64>], rows: &[Vec<i64>]) -> bool {
    if baselines.is_empty() {
        return false;
    }
    rows.iter()
        .enumerate()
        .all(|(index, row)| *row == baselines[index % baselines.len()])
}

fn measure(
    preprocessor: &FormulaPreprocessor,
    session: &mut FormulaSession,
    tokenizer: &FormulaTokenizer,
    images: &[image::DynamicImage],
) -> Result<Measurement, Box<dyn std::error::Error>> {
    let preprocess_start = Instant::now();
    let tensor = preprocessor.preprocess_batch(images)?;
    let preprocess_ms = ms(preprocess_start);

    let run_start = Instant::now();
    let output = session.run(tensor.view())?;
    let run_ms = ms(run_start);

    let decode_start = Instant::now();
    let mut tokens = Vec::with_capacity(images.len());
    for row in output.axis_iter(ndarray::Axis(0)) {
        let row = row.to_vec();
        let decoded = tokenizer.decode_ids(&row)?;
        tokens.push(eos_prefix(&decoded.token_ids, decoded.eos_index));
    }
    let decode_ms = ms(decode_start);

    Ok(Measurement {
        sample: RoundSample {
            preprocess_ms,
            run_ms,
            decode_ms,
            e2e_ms: preprocess_ms + run_ms + decode_ms,
        },
        tokens,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if cli.rounds == 0 {
        return Err("--rounds must be greater than zero".into());
    }
    if cli.batch_sizes.contains(&0) {
        return Err("--batch-sizes entries must be greater than zero".into());
    }

    let memory_start = peak_working_set_bytes();
    let runtime = runtime_config(&cli);
    let images = cli
        .images
        .iter()
        .map(image::open)
        .collect::<Result<Vec<_>, _>>()?;
    if images.is_empty() {
        return Err("--image requires at least one readable image".into());
    }
    let max_batch = images
        .len()
        .max(*cli.batch_sizes.iter().max().unwrap_or(&1));

    let session_start = Instant::now();
    let mut session = FormulaSession::new(&cli.model, &runtime)?;
    let session_create_ms = ms(session_start);
    let memory_after_session = peak_working_set_bytes();
    let resolution = session.provider_resolution();

    let character_metadata = session
        .character_metadata()?
        .ok_or("model has no `character` metadata")?;
    let metadata = FormulaTokenizerMetadata::from_character_metadata(&character_metadata)?;
    let tokenizer = FormulaTokenizer::from_metadata(&metadata)?;
    let preprocessor = FormulaPreprocessor::new();

    // 首次推理单独测量，不进入 warm 统计；同时为每张图建立“单图 token 基线”，
    // 用于判定 batch 是否改变结果（不能拿第一张的基线去比对其它图片的行）。
    // 基线与 `measure` 使用同一个 `eos_prefix` 口径，否则完整行的 padding 会被
    // 误报成结果差异。
    let first_input = preprocessor.preprocess(&images[0])?;
    let first_start = Instant::now();
    let first_output = session.run(first_input.view())?;
    let first_inference_ms = ms(first_start);
    let mut baselines: Vec<Vec<i64>> = Vec::with_capacity(images.len());
    let first_decoded = tokenizer.decode_ids(&first_output.row(0).to_vec())?;
    let baseline_latex = first_decoded.latex.clone();
    baselines.push(eos_prefix(
        &first_decoded.token_ids,
        first_decoded.eos_index,
    ));
    for image in images.iter().skip(1) {
        let input = preprocessor.preprocess(image)?;
        let output = session.run(input.view())?;
        let decoded = tokenizer.decode_ids(&output.row(0).to_vec())?;
        baselines.push(eos_prefix(&decoded.token_ids, decoded.eos_index));
    }

    // 预热。
    for _ in 0..cli.warmup {
        let _ = measure(&preprocessor, &mut session, &tokenizer, &images[..1])?;
    }

    // 单图 warm 轮次轮转所有图片，避免只测一张。
    let mut single_rounds = Vec::with_capacity(cli.rounds);
    for round in 0..cli.rounds {
        let image = &images[round % images.len()];
        let measurement = measure(
            &preprocessor,
            &mut session,
            &tokenizer,
            std::slice::from_ref(image),
        )?;
        single_rounds.push(measurement.sample);
    }

    let mut batches = Vec::new();
    for &batch in &cli.batch_sizes {
        if batch > max_batch {
            continue;
        }
        let selected: Vec<image::DynamicImage> = (0..batch)
            .map(|index| images[index % images.len()].clone())
            .collect();

        for _ in 0..cli.warmup {
            let _ = measure(&preprocessor, &mut session, &tokenizer, &selected)?;
        }

        let mut rounds = Vec::with_capacity(cli.rounds);
        let mut deterministic = true;
        for _ in 0..cli.rounds {
            // 复用本次测量解码出的 token 行做稳定性判定，避免额外推理扭曲耗时。
            let measurement = measure(&preprocessor, &mut session, &tokenizer, &selected)?;
            rounds.push(measurement.sample);
            if !batch_matches_baselines(&baselines, &measurement.tokens) {
                deterministic = false;
            }
        }

        batches.push(BatchReport {
            batch,
            per_image_e2e_ms: Stats::from_samples(
                rounds.iter().map(|r| r.e2e_ms / batch as f64).collect(),
            ),
            batch_e2e_ms: Stats::from_samples(rounds.iter().map(|r| r.e2e_ms).collect()),
            stages: StageStats::from_rounds(&rounds),
            deterministic_tokens: deterministic,
            latex: baseline_latex.clone(),
        });
    }

    let ocr_baseline = match &cli.ocr_baseline {
        Some(path) => Some(serde_json::from_str(&std::fs::read_to_string(path)?)?),
        None => None,
    };

    let memory_end = peak_working_set_bytes();
    let image_size = {
        use image::GenericImageView;
        let (width, height) = images[0].dimensions();
        Some([width, height])
    };

    let mut notes = vec![
        "公式模型的吞吐与普通 OCR 分开报告；`ocr_baseline` 仅作并列展示，不做加权合并。"
            .to_string(),
        "`deterministic_tokens` 逐行比较 batch 输出与同图单图推理的 token 序列；batch 不应改变结果。"
            .to_string(),
    ];
    if resolution.fallback_used {
        notes.push(
            "provider 发生 CPU 回退；benchmark 已用 fail_if_provider_unavailable=true，\
                    出现该状态说明环境与配置不一致。"
                .to_string(),
        );
    }
    if memory_end.is_none() {
        notes.push(format!(
            "峰值工作集采集失败：{}",
            rapid_ocr_rs::peak_memory_failure_reason()
                .unwrap_or_else(|| "GetProcessMemoryInfo 未返回数据".to_string())
        ));
    }

    let report = Report {
        tool: "formula_bench",
        provider_requested: format!("{:?}", cli.provider),
        provider_selected_ep: format!("{:?}", resolution.selected_ep),
        provider_fallback_used: resolution.fallback_used,
        rounds: cli.rounds,
        warmup: cli.warmup,
        model: ModelReport {
            path: cli.model.display().to_string(),
            file_name: cli
                .model
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            size_bytes: std::fs::metadata(&cli.model)?.len(),
            sha256: Some(sha256_file(&cli.model)?),
        },
        images: cli
            .images
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        image_size,
        threads: ThreadReport {
            intra_threads: runtime.intra_threads,
            inter_threads: runtime.inter_threads,
            auto_tune_threads: runtime.auto_tune_threads,
            logical_cpus: std::thread::available_parallelism().ok().map(|v| v.get()),
            physical_cpus: num_cpus::get_physical(),
        },
        session_create_ms,
        first_inference_ms,
        warm_single: StageStats::from_rounds(&single_rounds),
        batches,
        memory: MemoryReport {
            peak_working_set_start_bytes: memory_start,
            peak_working_set_after_session_bytes: memory_after_session,
            peak_working_set_end_bytes: memory_end,
            peak_working_set_delta_bytes: match (memory_start, memory_end) {
                (Some(start), Some(end)) => Some(end.saturating_sub(start)),
                _ => None,
            },
            source: peak_memory_source(),
        },
        ocr_baseline,
        notes,
    };

    let text = serde_json::to_string_pretty(&report)?;
    if let Some(path) = &cli.output {
        std::fs::write(path, &text)?;
    }
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Stats, batch_matches_baselines, eos_prefix};

    #[test]
    fn stats_are_wired_to_the_shared_implementation() {
        let stats = Stats::from_samples(vec![4.0, 1.0, 3.0, 2.0]);
        assert_eq!(stats.samples, 4);
        assert!((stats.mean_ms - 2.5).abs() < 1e-12);
        assert!((stats.p50_ms - 2.5).abs() < 1e-12);
        assert!(Stats::from_samples(Vec::new()).samples == 0);
    }

    /// EOS 截断：有 EOS 时保留到 EOS（含），无 EOS 时保留全部 token。
    #[test]
    fn eos_prefix_truncates_at_eos_and_keeps_unanchored_sequences() {
        assert_eq!(
            eos_prefix(&[0, 82, 1769, 2, 1, 1], Some(3)),
            vec![0, 82, 1769, 2]
        );
        assert_eq!(eos_prefix(&[0, 82, 1769], None), vec![0, 82, 1769]);
        // 越界 eos_index 不得 panic，按“无 EOS”处理。
        assert_eq!(eos_prefix(&[0, 1], Some(9)), vec![0, 1]);
        assert_eq!(eos_prefix(&[], None), Vec::<i64>::new());
    }

    /// batch 稳定性判定：必须按图片顺序与基线逐行比较，并且只比较 EOS 前内容。
    #[test]
    fn batch_determinism_compares_eos_prefixes_per_image() {
        let baselines = vec![vec![0, 82, 2], vec![0, 99, 2]];

        // 同图同序 -> 稳定。
        assert!(batch_matches_baselines(
            &baselines,
            &[vec![0, 82, 2], vec![0, 99, 2]]
        ));

        // 顺序错位必须被判为不稳定（不能拿第一张的基线比第二张的行）。
        assert!(!batch_matches_baselines(
            &baselines,
            &[vec![0, 99, 2], vec![0, 82, 2]]
        ));

        // 行数超过图片数时按 `index % len` 循环对齐（batch 复用图片）。
        assert!(batch_matches_baselines(
            &baselines,
            &[vec![0, 82, 2], vec![0, 99, 2], vec![0, 82, 2]]
        ));

        // token 内容变化必须被发现。
        assert!(!batch_matches_baselines(
            &baselines,
            &[vec![0, 82, 2], vec![0, 98, 2]]
        ));

        // 空基线集合无法判定，返回 false 而不是 panic。
        assert!(!batch_matches_baselines(&[], &[vec![0, 2]]));
    }

    /// 回归测试：batch 内被 `Loop` 补齐的 padding 不得造成“不稳定”的误判。
    ///
    /// 基线与测量都先做 EOS 截断，因此 batch=1 的短序列与 batch=2 中同图的长 padding
    /// 行必须判定为一致。
    #[test]
    fn padding_after_eos_does_not_count_as_a_difference() {
        let single = [0, 82, 1769, 2];
        let padded_batch_row = [0, 82, 1769, 2, 1, 1, 1, 1];
        let baselines = vec![eos_prefix(&single, Some(3))];
        let rows = vec![eos_prefix(&padded_batch_row, Some(3))];
        assert!(
            batch_matches_baselines(&baselines, &rows),
            "padding after EOS must be ignored"
        );
        // 如果直接比较未截断的行，就会得到相反（错误）的结论。
        assert_ne!(single.to_vec(), padded_batch_row.to_vec());
    }
}

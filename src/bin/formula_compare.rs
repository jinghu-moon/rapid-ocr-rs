use clap::Parser;
use rapid_ocr_rs::{
    FormulaRecognizer, RuntimeConfig,
    evaluation::formula::{
        fixture::{FormulaSplit, load_latex_ocr_example},
        metrics::evaluate_text,
    },
};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

#[derive(Debug, Parser)]
#[command(
    name = "formula_compare",
    about = "Run PP-FormulaNet_plus Rust inference over a formula fixture"
)]
struct Cli {
    #[arg(long)]
    model: PathBuf,
    #[arg(long = "dataset-root")]
    dataset_root: PathBuf,
    #[arg(long, default_value = "val")]
    split: String,
    #[arg(long, default_value_t = 0)]
    limit: usize,
    #[arg(long = "batch-size", default_value_t = 8)]
    batch_size: usize,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct Record {
    image: String,
    expected: String,
    actual: Option<String>,
    token_ids: Vec<i64>,
    eos_index: Option<usize>,
    truncated: Option<bool>,
    elapsed_ms: f32,
    exact_match: bool,
    normalized_match: bool,
    cer: f64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct Summary {
    count: usize,
    failed: usize,
    exact_match_rate: f64,
    normalized_match_rate: f64,
    mean_cer: f64,
    records: Vec<Record>,
}

fn error_record(image: String, expected: String, error: String, elapsed_ms: f32) -> Record {
    Record {
        image,
        expected,
        actual: None,
        token_ids: Vec::new(),
        eos_index: None,
        truncated: None,
        elapsed_ms,
        exact_match: false,
        normalized_match: false,
        cer: 1.0,
        error: Some(error),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let split = match cli.split.as_str() {
        "val" | "validate" => FormulaSplit::Validate,
        "train" => FormulaSplit::Train,
        other => return Err(format!("unsupported split `{other}` (expected val or train)").into()),
    };
    let fixture = load_latex_ocr_example(&cli.dataset_root, split)?;
    let mut recognizer = FormulaRecognizer::from_model(&cli.model, &RuntimeConfig::default())?;

    let samples: Vec<_> = fixture
        .scorable()
        .take(if cli.limit > 0 { cli.limit } else { usize::MAX })
        .collect();
    let batch_size = cli.batch_size.max(1);
    let mut records = Vec::new();
    let mut failed = 0usize;
    let mut exact = 0usize;
    let mut normalized = 0usize;
    let mut cer_sum = 0.0f64;
    let mut scored = 0usize;

    let mut index = 0usize;
    while index < samples.len() {
        let end = (index + batch_size).min(samples.len());
        let chunk = &samples[index..end];
        let mut chunk_records: Vec<Option<Record>> =
            std::iter::repeat_with(|| None).take(chunk.len()).collect();
        let mut valid_images = Vec::new();
        let mut valid_indices = Vec::new();

        for (local, sample) in chunk.iter().enumerate() {
            match image::open(&sample.image_path) {
                Ok(image) => {
                    valid_images.push(image);
                    valid_indices.push(local);
                }
                Err(error) => {
                    failed += 1;
                    chunk_records[local] = Some(error_record(
                        sample.image_path.to_string_lossy().to_string(),
                        sample.ground_truth.clone(),
                        format!("image decode failed: {error}"),
                        0.0,
                    ));
                }
            }
        }

        if !valid_images.is_empty() {
            let started = Instant::now();
            match recognizer.recognize_batch(&valid_images) {
                Ok(results) => {
                    for (result, local) in results.into_iter().zip(valid_indices) {
                        let sample = &chunk[local];
                        let metrics = evaluate_text(&sample.ground_truth, &result.latex);
                        exact += usize::from(metrics.exact_match);
                        normalized += usize::from(metrics.normalized_match);
                        cer_sum += metrics.cer;
                        scored += 1;
                        chunk_records[local] = Some(Record {
                            image: sample.image_path.to_string_lossy().to_string(),
                            expected: sample.ground_truth.clone(),
                            actual: Some(result.latex),
                            token_ids: result.token_ids,
                            eos_index: result.eos_index,
                            truncated: Some(result.truncated),
                            elapsed_ms: result.elapsed_ms,
                            exact_match: metrics.exact_match,
                            normalized_match: metrics.normalized_match,
                            cer: metrics.cer,
                            error: None,
                        });
                    }
                }
                Err(error) => {
                    let elapsed = started.elapsed().as_secs_f32() * 1000.0;
                    for local in valid_indices {
                        failed += 1;
                        let sample = &chunk[local];
                        chunk_records[local] = Some(error_record(
                            sample.image_path.to_string_lossy().to_string(),
                            sample.ground_truth.clone(),
                            error.to_string(),
                            elapsed,
                        ));
                    }
                }
            }
        }

        records.extend(chunk_records.into_iter().flatten());
        index = end;
    }

    let summary = Summary {
        count: records.len(),
        failed,
        exact_match_rate: if scored == 0 {
            0.0
        } else {
            exact as f64 / scored as f64
        },
        normalized_match_rate: if scored == 0 {
            0.0
        } else {
            normalized as f64 / scored as f64
        },
        mean_cer: if scored == 0 {
            0.0
        } else {
            cer_sum / scored as f64
        },
        records,
    };
    let text = serde_json::to_string_pretty(&summary)?;
    if let Some(path) = cli.output {
        std::fs::write(path, &text)?;
    }
    println!("{text}");
    Ok(())
}
